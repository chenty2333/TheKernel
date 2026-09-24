use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    num::NonZeroUsize,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    task::Context,
    time::Duration,
};
use std::{
    sync::{Barrier, Mutex as StdMutex, Once as StdOnce, mpsc},
    thread,
};

use axfs_ng_vfs::{
    AsyncVectoredWriteOutcome, DirEntry, FileNode, FileNodeOps, Filesystem, FilesystemOps, FsPath,
    Location, Metadata, MetadataUpdate, Mountpoint, NodeFlags, NodeOps, NodePermission, NodeType,
    NodeUserData, Reference, StatFs, VfsError, VfsResult,
};
use axio::{Cursor, IoBuf, IoBufMut, Read, Seek, SeekFrom, Write};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axsync::Mutex;
use lru::LruCache;

use super::{
    ALIGNED_BYPASS_CHUNK, CLOSED_FILE_CACHE_RETAINED_PAGES, CachedFile, CachedFileEvictionOwner,
    CachedFileReclaimStats, CachedFileShared, CachedPageEviction, CachedPageEvictionReservation,
    CachedPageInvalidationTransaction, FADVISE_NOREUSE, FADVISE_RANDOM,
    FADVISE_READAHEAD_QUEUE_CAPACITY, FADVISE_SEQUENTIAL, FadviseReadaheadQueue,
    FadviseReadaheadRequest, File, FileBackend, FileFlags, FileUserData,
    MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS, OpenOptions, PAGE_SIZE, PageCache, PhysicalIoSegment,
    PinnedPhysicalSegment, RANGE_CACHE_LEASE_SLOTS, RangeCacheLease, RangeCacheLeaseKind,
    WritePlacement, advance_clean_cached_file_reclaim_scan_epoch, begin_dirty_writeback_run,
    cached_file_registry_key, cached_file_shared_for_location,
    cached_file_shared_for_location_or_create, discard_cached_pages, fadvise_next_bits,
    file_cache_registry, finish_dirty_writeback_run, mark_cached_file_unlinked,
    physical_to_virtual, reclaim_clean_pages_from_shared,
    reclaim_clean_pages_from_shared_with_scan_budget,
    release_unlinked_cached_file_registry_ownership, remove_cached_file_registry_entry,
    stable_demote_lru_keys, synchronize_retained_page_count, try_collect_noreuse_keys,
    try_zeroed_pinned_io_bounce, validate_physical_io_segments, validate_pinned_physical_segments,
    with_cache_invalidating_file_operation_after_preflight,
    with_sync_and_invalidate_cached_file_pages,
};
#[cfg(feature = "ext4")]
use super::{PhysicalIoEffect, PhysicalIoResetProof};

/// The empty mount namespace makes the immutable nullfs the visible root,
/// so `open("/", O_RDONLY | O_DIRECTORY)` reaches a filesystem node that no
/// other boot path opens.  `NullDir` is a read-only directory whose root
/// inode is `S_IMMUTABLE` (fs/nullfs.c:7-34), and Linux still opens it.
///
/// Constructing the open file description then samples the node's
/// writeback errseq, so the root has to answer that like any other inode
/// of a trivial superblock instead of reporting `EOPNOTSUPP`.
#[test]
fn nullfs_root_directory_opens() {
    let fs = axfs_ng_vfs::nullfs::filesystem().unwrap();
    let root = Mountpoint::new_root(&fs);
    let context = crate::FsContext::new(root.root_location());
    let mut options = OpenOptions::new();
    options.read(true).directory(true);
    let opened = match options.open(&context, FsPath::new(b"/")) {
        Ok(opened) => opened,
        Err(error) => panic!("opening the nullfs root failed: {error:?}"),
    };
    let crate::OpenResult::Dir(directory) = opened else {
        panic!("the nullfs root opened as a non-directory");
    };
    let (location, _handle) = directory.into_parts();
    assert_eq!(location.node_type(), NodeType::Directory);
    let state = location
        .writeback_error_state()
        .expect("the nullfs root must expose the superblock errseq");
    assert_eq!(state.sample(), fs.writeback_error_state().sample());
    let filesystem = location.mountpoint().filesystem_handle();
    assert_eq!(state.sample(), filesystem.writeback_error_state().sample());
}

#[cfg(feature = "ext4")]
#[test]
fn prepared_physical_effect_is_worker_send() {
    // The effect is moved into one worker and settled through &mut self;
    // no shared reference crosses workers, so Sync is not its contract.
    fn assert_worker_move<T: Send + 'static>() {
        let work: Option<T> = None;
        std::thread::spawn(move || drop(work)).join().unwrap();
    }

    assert_worker_move::<PhysicalIoEffect>();
}

static PRESSURE_RECLAIM_EPOCH_TEST_LOCK: StdMutex<()> = StdMutex::new(());

struct TestEvictionReservation;

impl CachedPageEvictionReservation for TestEvictionReservation {
    fn commit(self: Box<Self>) {}

    fn abort(self: Box<Self>) {}
}

fn prepared_listener() -> VfsResult<Box<dyn CachedPageEvictionReservation>> {
    Ok(Box::new(TestEvictionReservation))
}

#[test]
fn fadvise_readahead_ring_is_fixed_capacity_and_preserves_fifo_across_restart() {
    let mut queue = FadviseReadaheadQueue::new();
    for offset in 0..FADVISE_READAHEAD_QUEUE_CAPACITY {
        assert!(queue.push(FadviseReadaheadRequest {
            offset: offset as u64,
            len: PAGE_SIZE as u64,
        }));
    }
    assert!(!queue.push(FadviseReadaheadRequest {
        offset: u64::MAX,
        len: PAGE_SIZE as u64,
    }));
    assert!(queue.contains(FadviseReadaheadRequest {
        offset: 0,
        len: PAGE_SIZE as u64,
    }));

    // A failed spawn clears its published running state; pending work
    // stays in the fixed ring for a later caller to restart.
    queue.worker_running = true;
    queue.worker_running = false;
    for offset in 0..FADVISE_READAHEAD_QUEUE_CAPACITY {
        assert_eq!(
            queue.pop(),
            Some(FadviseReadaheadRequest {
                offset: offset as u64,
                len: PAGE_SIZE as u64,
            })
        );
    }
    assert_eq!(queue.pop(), None);

    // Exercise the wrapped tail too, without allocating a replacement
    // backing store.
    assert!(queue.push(FadviseReadaheadRequest { offset: 7, len: 1 }));
    assert_eq!(
        queue.pop(),
        Some(FadviseReadaheadRequest { offset: 7, len: 1 })
    );
}

#[test]
fn fadvise_random_and_sequential_are_exclusive_while_noreuse_persists() {
    let sequential = fadvise_next_bits(FADVISE_NOREUSE, FADVISE_SEQUENTIAL, FADVISE_RANDOM);
    assert_eq!(sequential, FADVISE_SEQUENTIAL | FADVISE_NOREUSE);
    let random = fadvise_next_bits(sequential, FADVISE_RANDOM, FADVISE_SEQUENTIAL);
    assert_eq!(random, FADVISE_RANDOM | FADVISE_NOREUSE);
    assert_eq!(fadvise_next_bits(random, 0, 0), random);
}

#[test]
fn direct_fadvise_without_cached_inode_is_a_nonallocating_noop() {
    let fs = Filesystem::new(RegistryTestFs::new());
    let mountpoint = Mountpoint::new_root(&fs);
    let location = mountpoint.root_location();
    assert!(cached_file_shared_for_location(&location).is_none());

    let direct = FileBackend::new_direct(location.clone());
    direct.fadvise_willneed(0, PAGE_SIZE as u64).unwrap();
    direct.fadvise_noreuse(0, PAGE_SIZE as u64).unwrap();
    direct.fadvise_dontneed(0, PAGE_SIZE as u64).unwrap();

    assert!(cached_file_shared_for_location(&location).is_none());
}

#[test]
fn noreuse_cold_splice_is_a_stable_lru_partition() {
    let mut cache = LruCache::new(NonZeroUsize::new(4).unwrap());
    for pn in [1, 2, 3, 4] {
        cache.put(pn, pn);
    }
    // LRU order before the splice is 1, 2, 3, 4.
    stable_demote_lru_keys(&mut cache, &[1, 3]);
    let lru_order: Vec<_> = cache.iter().rev().map(|(pn, _)| *pn).collect();
    assert_eq!(lru_order, [1, 3, 2, 4]);
}

#[test]
fn noreuse_reprioritization_oom_is_best_effort() {
    let cache = LruCache::<u32, u32>::new(NonZeroUsize::new(1).unwrap());
    // This deterministically exercises Vec's fallible reservation rather
    // than relying on ambient allocator pressure.
    assert!(try_collect_noreuse_keys(&cache, 0, PAGE_SIZE as u64, usize::MAX).is_none());
}

#[test]
fn range_cache_leases_enforce_overlap_modes_and_allow_disjoint_direct_io() {
    let shared = Arc::new(CachedFileShared::new(
        super::CachedFileIdentity {
            device: 1,
            inode: 2,
            object: 3,
        },
        false,
    ));
    let cached = CachedFileShared::try_range_cache_lease(
        &shared,
        0..PAGE_SIZE as u64,
        RangeCacheLeaseKind::CachedWrite,
    )
    .unwrap();
    assert!(matches!(
        CachedFileShared::try_range_cache_lease(
            &shared,
            PAGE_SIZE as u64 / 2..PAGE_SIZE as u64 * 2,
            RangeCacheLeaseKind::DirectWrite,
        ),
        Err(VfsError::ResourceBusy)
    ));
    let disjoint = CachedFileShared::try_range_cache_lease(
        &shared,
        PAGE_SIZE as u64 * 2..PAGE_SIZE as u64 * 3,
        RangeCacheLeaseKind::DirectRead,
    )
    .unwrap();
    assert!(cached.revalidate());
    assert!(disjoint.revalidate());
    drop(disjoint);
    drop(cached);
    assert!(
        CachedFileShared::try_range_cache_lease(
            &shared,
            0..PAGE_SIZE as u64,
            RangeCacheLeaseKind::DirectRead,
        )
        .is_ok()
    );
}

#[test]
fn only_direct_range_lease_drop_requests_unlinked_cleanup() {
    assert!(super::range_lease_drop_requests_unlinked_cleanup(
        RangeCacheLeaseKind::DirectRead
    ));
    assert!(super::range_lease_drop_requests_unlinked_cleanup(
        RangeCacheLeaseKind::DirectWrite
    ));
    assert!(!super::range_lease_drop_requests_unlinked_cleanup(
        RangeCacheLeaseKind::CachedRead
    ));
    assert!(!super::range_lease_drop_requests_unlinked_cleanup(
        RangeCacheLeaseKind::CachedWrite
    ));
    assert!(!super::range_lease_drop_requests_unlinked_cleanup(
        RangeCacheLeaseKind::WholeFileMutation
    ));
}

#[test]
fn range_cache_lease_capacity_is_bounded_and_stale_generation_cannot_clear_reuse() {
    let shared = Arc::new(CachedFileShared::new(
        super::CachedFileIdentity {
            device: 3,
            inode: 4,
            object: 5,
        },
        false,
    ));
    let mut leases = Vec::new();
    for index in 0..RANGE_CACHE_LEASE_SLOTS {
        leases.push(
            CachedFileShared::try_range_cache_lease(
                &shared,
                index as u64 * PAGE_SIZE as u64..(index as u64 + 1) * PAGE_SIZE as u64,
                RangeCacheLeaseKind::DirectRead,
            )
            .unwrap(),
        );
    }
    assert!(matches!(
        CachedFileShared::try_range_cache_lease(
            &shared,
            128 * PAGE_SIZE as u64..129 * PAGE_SIZE as u64,
            RangeCacheLeaseKind::DirectRead,
        ),
        Err(VfsError::ResourceBusy)
    ));
    let stale = RangeCacheLease {
        shared: shared.clone(),
        slot: leases[0].slot,
        generation: leases[0].generation,
        record: leases[0].record,
    };
    drop(leases.remove(0));
    let replacement = CachedFileShared::try_range_cache_lease(
        &shared,
        0..PAGE_SIZE as u64,
        RangeCacheLeaseKind::DirectWrite,
    )
    .unwrap();
    drop(stale);
    assert!(replacement.revalidate());
}

#[cfg(feature = "ext4")]
#[test]
fn physical_reset_proof_rejects_unproven_quarantine() {
    use axdriver::prelude::BlockResetOutcome;

    assert!(PhysicalIoResetProof::from_lower_reset(BlockResetOutcome::Quiesced).is_some());
    assert!(PhysicalIoResetProof::from_lower_reset(BlockResetOutcome::Retired).is_some());
    assert!(PhysicalIoResetProof::from_lower_reset(BlockResetOutcome::Quarantined).is_none());
}

#[cfg(feature = "ext4")]
#[test]
fn prepared_physical_effect_rolls_back_cache_before_direct_lease_cleanup() {
    init_test_page_allocator();
    let shared = Arc::new(CachedFileShared::new(
        super::CachedFileIdentity {
            device: 9,
            inode: 10,
            object: 11,
        },
        false,
    ));
    let mut page = PageCache::new(false).unwrap();
    page.data().fill(0x5a);
    assert!(shared.page_cache.lock().put(0, page).is_none());
    shared.unlinked.store(true, Ordering::Release);

    let mut invalidation = CachedPageInvalidationTransaction::new_shared(shared.clone());
    assert_eq!(invalidation.stage_all(), Ok(()));
    let mut range_lease = Some(
        CachedFileShared::try_range_cache_lease(
            &shared,
            0..PAGE_SIZE as u64,
            RangeCacheLeaseKind::DirectWrite,
        )
        .unwrap(),
    );
    let mut invalidation = Some(invalidation);

    super::drop_prepared_physical_effect_owners(&mut invalidation, &mut range_lease);

    assert!(invalidation.is_none());
    assert!(range_lease.is_none());
    assert!(shared.page_cache.lock().is_empty());
}

#[test]
fn range_cache_lease_slots_recycle_across_repeated_reset_cycles() {
    let shared = Arc::new(CachedFileShared::new(
        super::CachedFileIdentity {
            device: 6,
            inode: 7,
            object: 8,
        },
        false,
    ));
    for cycle in 0..(RANGE_CACHE_LEASE_SLOTS * 4) {
        let start = (cycle % RANGE_CACHE_LEASE_SLOTS) as u64 * PAGE_SIZE as u64;
        let lease = CachedFileShared::try_range_cache_lease(
            &shared,
            start..start + PAGE_SIZE as u64,
            RangeCacheLeaseKind::DirectRead,
        )
        .unwrap();
        assert!(lease.revalidate());
        drop(lease);
    }
    assert!(
        shared
            .range_cache_leases
            .lock()
            .slots
            .iter()
            .all(Option::is_none)
    );
}

#[test]
fn physical_sg_validation_requires_nonempty_aligned_disjoint_ranges() {
    let valid = [
        PhysicalIoSegment::new(512, 512),
        PhysicalIoSegment::new(4096, 1024),
    ];
    assert_eq!(validate_physical_io_segments(&valid, 0), Ok(1536));
    assert_eq!(
        validate_physical_io_segments(&[], 0),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(
        validate_physical_io_segments(&[PhysicalIoSegment::new(512, 0)], 0),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(
        validate_physical_io_segments(
            &[
                PhysicalIoSegment::new(512, 1024),
                PhysicalIoSegment::new(1024, 512),
            ],
            0,
        ),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(
        validate_physical_io_segments(&[PhysicalIoSegment::new(512, 512)], 1),
        Err(VfsError::InvalidInput)
    );
}

#[test]
fn default_physical_hook_returns_fallback_without_file_io() {
    let (file, state) = append_test_file_with_access(
        NodeFlags::NON_CACHEABLE,
        4096,
        FileFlags::READ | FileFlags::WRITE,
    );
    let segments = [PhysicalIoSegment::new(512, 512)];
    assert_eq!(
        unsafe { file.try_read_at_dma_segments(&segments, 0) },
        Ok(None)
    );
    assert_eq!(
        unsafe { file.try_write_at_dma_segments(&segments, 0) },
        Ok(None)
    );
    assert_eq!(state.read_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.write_calls.load(Ordering::Acquire), 0);
}

#[test]
fn physical_preflight_rejects_before_cache_mutation_or_operation() {
    let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x5a, true);
    let reads_before = state.read_calls.load(Ordering::Acquire);
    let preflight_calls = AtomicUsize::new(0);
    let operation_calls = AtomicUsize::new(0);
    let result = with_cache_invalidating_file_operation_after_preflight(
        &location,
        |_, _| {
            preflight_calls.fetch_add(1, Ordering::AcqRel);
            Ok(false)
        },
        |_, _| {
            operation_calls.fetch_add(1, Ordering::AcqRel);
            Ok(())
        },
    );
    assert!(matches!(result, Ok(None)));
    assert_eq!(preflight_calls.load(Ordering::Acquire), 1);
    assert_eq!(operation_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.read_calls.load(Ordering::Acquire), reads_before);
    assert_eq!(state.write_calls.load(Ordering::Acquire), 0);
    cached.with_page(0, |page| {
        let page = page.expect("preflight rejection discarded a cached page");
        assert!(page.is_dirty());
    });
}

#[test]
fn no_data_open_options_emit_no_data_access_flags() {
    let mut options = OpenOptions::new();
    options.no_data(true).direct(true).no_atime(true);
    let flags = options.to_flags().unwrap();
    assert!(!flags.intersects(FileFlags::READ | FileFlags::WRITE | FileFlags::PATH));
    assert!(flags.contains(FileFlags::DIRECT | FileFlags::NOATIME));

    options.read(true);
    assert!(!options.is_valid());
}

struct AppendTestState {
    read_offsets: Mutex<Vec<u64>>,
    write_offsets: Mutex<Vec<u64>>,
    read_calls: AtomicUsize,
    async_read_calls: AtomicUsize,
    async_write_mode: AtomicU8,
    write_calls: AtomicUsize,
    append_calls: AtomicUsize,
    open_calls: AtomicUsize,
    set_len_calls: AtomicUsize,
    last_read_buf: AtomicUsize,
    last_write_buf: AtomicUsize,
    inode_len: AtomicU64,
    append_limit: AtomicUsize,
    fail_read_call: AtomicUsize,
    fail_write_call: AtomicUsize,
    fail_append_call: AtomicUsize,
    fail_set_len_call: AtomicUsize,
    yield_after_append: AtomicBool,
    fail_set_len: AtomicBool,
    set_len_failure_atomic: AtomicBool,
    full_page_io: AtomicBool,
    stored_first_byte: AtomicU8,
    append_markers: StdMutex<Vec<u8>>,
    user_data: NodeUserData,
}

impl AppendTestState {
    fn new(inode_len: u64) -> Arc<Self> {
        Arc::new(Self {
            read_offsets: Mutex::new(Vec::new()),
            write_offsets: Mutex::new(Vec::new()),
            read_calls: AtomicUsize::new(0),
            async_read_calls: AtomicUsize::new(0),
            async_write_mode: AtomicU8::new(0),
            write_calls: AtomicUsize::new(0),
            append_calls: AtomicUsize::new(0),
            open_calls: AtomicUsize::new(0),
            set_len_calls: AtomicUsize::new(0),
            last_read_buf: AtomicUsize::new(0),
            last_write_buf: AtomicUsize::new(0),
            inode_len: AtomicU64::new(inode_len),
            append_limit: AtomicUsize::new(usize::MAX),
            fail_read_call: AtomicUsize::new(usize::MAX),
            fail_write_call: AtomicUsize::new(usize::MAX),
            fail_append_call: AtomicUsize::new(usize::MAX),
            fail_set_len_call: AtomicUsize::new(usize::MAX),
            yield_after_append: AtomicBool::new(false),
            fail_set_len: AtomicBool::new(false),
            set_len_failure_atomic: AtomicBool::new(true),
            full_page_io: AtomicBool::new(false),
            stored_first_byte: AtomicU8::new(0),
            append_markers: StdMutex::new(Vec::new()),
            user_data: NodeUserData::new(),
        })
    }
}

struct AppendTestFile {
    flags: NodeFlags,
    state: Arc<AppendTestState>,
    fs: Arc<RegistryTestFs>,
}

impl NodeOps for AppendTestFile {
    fn inode(&self) -> u64 {
        1
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        Ok(Metadata {
            device: 0,
            inode: 1,
            nlink: 1,
            mode: NodePermission::from_bits_truncate(0o600),
            node_type: NodeType::RegularFile,
            uid: 0,
            gid: 0,
            project_id: 0,
            size: self.state.inode_len.load(Ordering::Acquire),
            block_size: 4096,
            blocks: 0,
            rdev: Default::default(),
            atime: axfs_ng_vfs::Timestamp::ZERO,
            btime: axfs_ng_vfs::Timestamp::ZERO,
            mtime: axfs_ng_vfs::Timestamp::ZERO,
            ctime: axfs_ng_vfs::Timestamp::ZERO,
        })
    }

    fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
        Ok(())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        &*self.fs
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn open(&self, _read: bool, _write: bool) -> VfsResult<()> {
        self.state.open_calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        self.flags
    }

    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(&self.state.user_data)
    }
}

impl Pollable for AppendTestFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}

impl FileNodeOps for AppendTestFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.state
            .last_read_buf
            .store(buf.as_ptr() as usize, Ordering::Release);
        self.state.read_offsets.lock().push(offset);
        let call = self.state.read_calls.fetch_add(1, Ordering::AcqRel);
        if call == self.state.fail_read_call.load(Ordering::Acquire) {
            return Err(VfsError::InvalidInput);
        }
        if self.state.full_page_io.load(Ordering::Acquire) {
            let len = self
                .state
                .inode_len
                .load(Ordering::Acquire)
                .saturating_sub(offset)
                .min(buf.len() as u64) as usize;
            buf[..len].fill(0);
            if offset == 0 && len != 0 {
                buf[0] = self.state.stored_first_byte.load(Ordering::Acquire);
            }
            return Ok(len);
        }
        let data = b"abcdefgh";
        let offset = usize::try_from(offset).map_err(|_| VfsError::InvalidInput)?;
        if offset >= data.len() {
            return Ok(0);
        }
        let read = buf.len().min(data.len() - offset);
        buf[..read].copy_from_slice(&data[offset..offset + read]);
        Ok(read)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.state
            .last_write_buf
            .store(buf.as_ptr() as usize, Ordering::Release);
        self.state.write_offsets.lock().push(offset);
        let call = self.state.write_calls.fetch_add(1, Ordering::AcqRel);
        if call == self.state.fail_write_call.load(Ordering::Acquire) {
            return Err(VfsError::InvalidInput);
        }
        if self.state.full_page_io.load(Ordering::Acquire) {
            if offset == 0
                && let Some(first) = buf.first()
            {
                self.state
                    .stored_first_byte
                    .store(*first, Ordering::Release);
            }
            return Ok(buf.len());
        }
        if offset != 0 {
            return Err(VfsError::InvalidInput);
        }
        Ok(buf.len().min(2))
    }

    fn try_read_at_vectored_async(
        &self,
        _bufs: &mut [&mut [u8]],
        _offset: u64,
    ) -> VfsResult<Option<usize>> {
        self.state.async_read_calls.fetch_add(1, Ordering::AcqRel);
        Ok(None)
    }

    fn try_write_at_vectored_async(
        &self,
        _bufs: &[&[u8]],
        _offset: u64,
    ) -> VfsResult<AsyncVectoredWriteOutcome> {
        match self.state.async_write_mode.load(Ordering::Acquire) {
            1 => Err(VfsError::Io),
            2 => Ok(AsyncVectoredWriteOutcome::CompletionError(VfsError::Io)),
            3 => Ok(AsyncVectoredWriteOutcome::Completed(PAGE_SIZE)),
            _ => Ok(AsyncVectoredWriteOutcome::NotSubmitted),
        }
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        let call = self.state.append_calls.fetch_add(1, Ordering::AcqRel);
        if call == self.state.fail_append_call.load(Ordering::Acquire) {
            return Err(VfsError::InvalidInput);
        }
        if let Some(marker) = buf.first().copied() {
            self.state.append_markers.lock().unwrap().push(marker);
        }
        let written = buf
            .len()
            .min(self.state.append_limit.load(Ordering::Acquire));
        let old_len = self
            .state
            .inode_len
            .fetch_add(written as u64, Ordering::AcqRel);
        if self.state.yield_after_append.load(Ordering::Acquire) {
            thread::yield_now();
        }
        Ok((written, old_len + written as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        let call = self.state.set_len_calls.fetch_add(1, Ordering::AcqRel);
        if self.state.fail_set_len.load(Ordering::Acquire)
            || call == self.state.fail_set_len_call.load(Ordering::Acquire)
        {
            if !self.state.set_len_failure_atomic.load(Ordering::Acquire) {
                self.state.inode_len.store(len, Ordering::Release);
            }
            return Err(VfsError::InvalidInput);
        }
        self.state.inode_len.store(len, Ordering::Release);
        Ok(())
    }

    fn set_len_failure_is_atomic(&self) -> bool {
        self.state.set_len_failure_atomic.load(Ordering::Acquire)
    }

    fn set_symlink(&self, _target: &FsPath) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }
}

fn append_test_file_with_access(
    flags: NodeFlags,
    inode_len: u64,
    access: FileFlags,
) -> (File, Arc<AppendTestState>) {
    let state = AppendTestState::new(inode_len);
    let fs = Filesystem::new(RegistryTestFs::new_for_append(flags, state.clone()));
    let mountpoint = Mountpoint::new_root(&fs);
    let location = mountpoint.root_location();
    let file = File::new(FileBackend::Direct(location), access);
    (file, state)
}

fn append_test_file(flags: NodeFlags, inode_len: u64) -> (File, Arc<AppendTestState>) {
    append_test_file_with_access(flags, inode_len, FileFlags::WRITE | FileFlags::APPEND)
}

fn init_test_page_allocator() {
    static PAGE_ALLOCATOR_INIT: StdOnce = StdOnce::new();
    PAGE_ALLOCATOR_INIT.call_once(|| {
        const TEST_PAGE_MEMORY: usize = 16 * 1024 * 1024;
        const PAGE_ALLOCATOR_BASE_ALIGN: usize = 1024 * 1024 * 1024;
        let layout =
            std::alloc::Layout::from_size_align(TEST_PAGE_MEMORY, PAGE_ALLOCATOR_BASE_ALIGN)
                .unwrap();
        let memory = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!memory.is_null());
        axalloc::global_init(memory as usize, TEST_PAGE_MEMORY);
    });
}

fn cached_append_test_file(inode_len: u64) -> (CachedFile, Location, Arc<AppendTestState>) {
    init_test_page_allocator();

    let state = AppendTestState::new(inode_len);
    let fs = Filesystem::new(RegistryTestFs::new_for_append(
        NodeFlags::empty(),
        state.clone(),
    ));
    let location = Mountpoint::new_root(&fs).root_location();
    let cached = CachedFile::get_or_create(location.clone());
    (cached, location, state)
}

#[test]
fn in_memory_backing_preserves_more_than_1024_faulted_pages() {
    init_test_page_allocator();
    // Three 800x600 XRGB buffers share one file in the Wayland client.
    const PAGES: u32 = 1407;
    let state = AppendTestState::new(u64::from(PAGES) * PAGE_SIZE as u64);
    let fs = Filesystem::new(RegistryTestFs::new_for_append(
        NodeFlags::ALWAYS_CACHE,
        state,
    ));
    let location = Mountpoint::new_root(&fs).root_location();
    let cached = CachedFile::get_or_create(location.clone());
    assert!(cached.in_memory());
    let mut physical = Vec::new();
    for pn in 0..PAGES {
        let paddr = cached
            .with_page_or_insert_without_reclaim(pn, |page| {
                page.data().fill((pn % 251) as u8);
                page.mark_dirty();
                Ok(page.paddr())
            })
            .expect("in-memory contents must grow past a disk-cache eviction limit");
        physical.push(paddr);
    }
    assert_eq!(
        cached.cachestat(0, u64::from(PAGES - 1)).nr_cache,
        u64::from(PAGES)
    );
    assert_eq!(cached.reclaim_one(), Ok(false));
    for pn in 0..PAGES {
        cached.with_page(pn, |page| {
            let page = page.expect("no earlier backing page may be evicted");
            assert_eq!(page.paddr(), physical[pn as usize]);
            assert!(page.data().iter().all(|byte| *byte == (pn % 251) as u8));
        });
    }
    mark_cached_file_unlinked(&location);
    drop(cached);
}

#[test]
fn reclaim_contention_preserves_pages_and_retries_after_reader_releases() {
    let (cached, location, _) = cached_append_test_file((2 * PAGE_SIZE) as u64);
    cached
        .with_page_or_insert_without_reclaim(0, |page| {
            page.data()[0] = 0x5a;
            Ok(())
        })
        .unwrap();
    let reader = cached.begin_cache_user().unwrap();
    assert_eq!(cached.reclaim_one(), Err(VfsError::ResourceBusy));
    cached.with_page(0, |page| assert_eq!(page.unwrap().data()[0], 0x5a));
    drop(reader);
    assert_eq!(cached.reclaim_one(), Ok(true));
    cached.with_page(0, |page| assert!(page.is_none()));
    assert_eq!(cached.reclaim_one(), Ok(false));
    mark_cached_file_unlinked(&location);
    drop(cached);
}

#[test]
fn ordinary_cache_still_requires_reclaim_at_its_bounded_capacity() {
    let (cached, location, _) = cached_append_test_file(64 * 1024 * 1024);
    assert!(!cached.in_memory());
    let capacity = cached.shared.page_cache.lock().cap().get();
    assert_eq!(capacity, super::per_file_page_cache_capacity().get());
    for pn in 0..capacity as u32 {
        cached
            .with_page_or_insert_without_reclaim(pn, |page| {
                page.data()[0] = 0x5a;
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(
        cached.with_page_or_insert_without_reclaim(capacity as u32, |_| Ok(())),
        Err(VfsError::ResourceBusy)
    );
    assert_eq!(cached.shared.page_cache.lock().len(), capacity);
    assert!(cached.shared.page_cache.lock().contains(&0));
    mark_cached_file_unlinked(&location);
    drop(cached);
}

fn seed_cached_page(cached: &CachedFile, pn: u32, byte: u8, dirty: bool) -> axhal::mem::PhysAddr {
    let mut paddr = None;
    cached
        .with_page_or_insert(pn, |page, evicted| {
            assert!(evicted.is_none());
            page.data().fill(byte);
            if dirty {
                page.mark_dirty();
            }
            paddr = Some(page.paddr());
            Ok(())
        })
        .unwrap();
    paddr.unwrap()
}

#[test]
fn cold_pages_demotes_only_resident_cache_entries() {
    let (cached, _location, _state) = cached_append_test_file(2 * PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x11, false);
    seed_cached_page(&cached, 1, 0x22, false);
    cached.with_page(0, |_| {});
    assert_eq!(cached.shared.page_cache.lock().peek_lru().unwrap().0, &1);

    assert_eq!(cached.cold_pages(0..1).unwrap(), 1);
    assert_eq!(cached.shared.page_cache.lock().peek_lru().unwrap().0, &0);
    assert_eq!(cached.cold_pages(2..3).unwrap(), 0);
}

#[test]
fn aligned_direct_page_range_rejects_byte_and_page_number_overflow() {
    assert!(CachedFile::aligned_page_range(u64::MAX - PAGE_SIZE as u64 + 1, PAGE_SIZE).is_none());
    let first_unrepresentable_page = (u64::from(u32::MAX) + 1) * PAGE_SIZE as u64;
    assert!(CachedFile::aligned_page_range(first_unrepresentable_page, PAGE_SIZE).is_none());
}

#[test]
fn pageout_writes_back_then_evicts_each_resident_page() {
    let (cached, _location, state) = cached_append_test_file(2 * PAGE_SIZE as u64);
    state.full_page_io.store(true, Ordering::Release);
    seed_cached_page(&cached, 0, 0x31, true);
    seed_cached_page(&cached, 1, 0x32, false);
    let notifications = Arc::new(AtomicUsize::new(0));
    let notified = notifications.clone();
    let handle = cached
        .add_evict_listener(CachedFileEvictionOwner::new(7).unwrap(), move |_| {
            notified.fetch_add(1, Ordering::AcqRel);
            prepared_listener()
        })
        .unwrap();

    assert_eq!(cached.pageout_pages(0..3).unwrap(), 2);
    assert_eq!(state.write_calls.load(Ordering::Acquire), 1);
    assert_eq!(notifications.load(Ordering::Acquire), 2);
    assert!(!cached.shared.page_cache.lock().contains(&0));
    assert!(!cached.shared.page_cache.lock().contains(&1));
    let stat = cached.cachestat(0, 2);
    // The committed PAGEOUT state is a clean nonresident shadow: a
    // successful writeback may not leave stale resident, dirty, or
    // writeback accounting behind it.
    assert_eq!(stat.nr_cache, 0);
    assert_eq!(stat.nr_dirty, 0);
    assert_eq!(stat.nr_writeback, 0);
    assert_eq!(stat.nr_evicted, 2);

    unsafe { cached.remove_evict_listener(handle) };
}

#[test]
fn pageout_listener_failure_restores_dirty_page_without_shadow() {
    let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
    let original_paddr = seed_cached_page(&cached, 0, 0x5a, true);
    let foreign = CachedFileEvictionOwner::new(8).unwrap();
    let handle = cached
        .add_evict_listener(foreign, |_| Err(VfsError::ResourceBusy))
        .unwrap();

    assert_eq!(cached.pageout_pages(0..1), Err(VfsError::ResourceBusy));
    // Alias preparation must succeed before any backing write starts.
    assert_eq!(state.write_calls.load(Ordering::Acquire), 0);
    assert_eq!(cached.cachestat(0, 0).nr_cache, 1);
    assert_eq!(cached.cachestat(0, 0).nr_dirty, 1);
    assert_eq!(cached.cachestat(0, 0).nr_evicted, 0);
    cached.with_page(0, |page| {
        let page = page.expect("failed pageout must restore its cache page");
        assert_eq!(page.paddr(), original_paddr);
        assert!(page.is_dirty());
        assert!(page.data().iter().all(|byte| *byte == 0x5a));
    });

    unsafe { cached.remove_evict_listener(handle) };
}

#[test]
fn pageout_rollback_is_repeatable_without_cache_or_shadow_drift() {
    let (cached, _location, _state) = cached_append_test_file(2 * PAGE_SIZE as u64);
    let original_paddr = seed_cached_page(&cached, 0, 0x6b, true);
    seed_cached_page(&cached, 1, 0x6c, false);
    // Promote the page so rollback exercises both resident and active
    // accounting, not merely the common inactive case.
    cached.with_page(0, |page| assert!(page.is_some()));
    assert!(cached.shared.page_cache.lock().get(&0).unwrap().is_active());
    assert_eq!(cached.shared.page_cache.lock().peek_lru().unwrap().0, &1);
    let mutation = cached.begin_cache_invalidating_mutation().unwrap();

    for _ in 0..1000 {
        let mut pageout = CachedPageInvalidationTransaction::new_pageout(&mutation);
        assert!(pageout.stage_page_for_pageout(0).unwrap());
        let staged = cached.cachestat(0, 0);
        assert_eq!(staged.nr_cache, 0);
        assert_eq!(staged.nr_dirty, 0);
        assert_eq!(staged.nr_writeback, 0);
        drop(pageout);
        let restored = cached.cachestat(0, 0);
        assert_eq!(restored.nr_cache, 1);
        assert_eq!(restored.nr_dirty, 1);
        assert_eq!(restored.nr_writeback, 0);
        assert_eq!(restored.nr_evicted, 0);
        assert_eq!(cached.shared.page_cache.lock().peek_lru().unwrap().0, &1);
    }

    drop(mutation);
    cached.with_page(0, |page| {
        let page = page.unwrap();
        assert_eq!(page.paddr(), original_paddr);
        assert!(page.is_dirty());
        assert!(page.is_active());
        assert!(page.data().iter().all(|byte| *byte == 0x6b));
    });
}

#[test]
fn range_invalidation_rollback_restores_exact_lru_order() {
    let (cached, _location, _state) = cached_append_test_file(3 * PAGE_SIZE as u64);
    for pn in 0..3 {
        seed_cached_page(&cached, pn, 0x40 + pn as u8, false);
    }
    // Establish a non-insertion order so a rollback cannot accidentally
    // pass by restoring every page at the cold end.
    cached.with_page(1, |_| {});
    let before: Vec<_> = cached
        .shared
        .page_cache
        .lock()
        .iter()
        .map(|(pn, _)| *pn)
        .collect();
    let owner = CachedFileEvictionOwner::new(9).unwrap();
    let handle = cached
        .add_evict_listener(owner, |_| Err(VfsError::ResourceBusy))
        .unwrap();
    let mutation = cached.begin_cache_invalidating_mutation().unwrap();
    {
        let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
        invalidation.stage_range(0..3).unwrap();
        assert_eq!(
            invalidation.prepare_evictions(),
            Err(VfsError::ResourceBusy)
        );
    }
    let after: Vec<_> = cached
        .shared
        .page_cache
        .lock()
        .iter()
        .map(|(pn, _)| *pn)
        .collect();
    assert_eq!(after, before);
    unsafe { cached.remove_evict_listener(handle) };
}

#[test]
fn async_dirty_writeback_completion_error_is_published_on_the_backend_inode() {
    let (_cached, location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    let writeback_errors = location.writeback_error_state().unwrap();
    let mut cursor = writeback_errors.sample();
    let file = location.entry().as_file().unwrap();

    super::publish_async_dirty_writeback_completion_error(file, VfsError::Io);

    assert_eq!(
        writeback_errors.check_and_advance(&mut cursor),
        Some(VfsError::Io)
    );
}

#[test]
fn async_dirty_writeback_presubmit_error_is_not_published() {
    let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x5a, true);
    let writeback_errors = location.writeback_error_state().unwrap();
    let mut cursor = writeback_errors.sample();
    let file = location.entry().as_file().unwrap();

    state.async_write_mode.store(1, Ordering::Release);
    axdriver::set_virtio_async_block_enabled(true);
    super::set_async_dirty_flush_sg_enabled(true);
    let result = cached.flush_dirty_cache(file);
    super::set_async_dirty_flush_sg_enabled(false);
    axdriver::set_virtio_async_block_enabled(false);

    assert_eq!(result, Err(VfsError::Io));
    assert_eq!(writeback_errors.check_and_advance(&mut cursor), None);
}

#[test]
fn range_sg_completion_error_arrives_with_its_errseq_already_published() {
    // The SG path admits at least two adjacent complete pages.
    let (cached, location, state) = cached_append_test_file(2 * PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x5a, true);
    seed_cached_page(&cached, 1, 0x5b, true);
    let writeback_errors = location.writeback_error_state().unwrap();
    let mut cursor = writeback_errors.sample();

    state.async_write_mode.store(2, Ordering::Release);
    axdriver::set_virtio_async_block_enabled(true);
    super::set_async_dirty_flush_sg_enabled(true);
    let result = cached.sync_range_marked(0, 2 * PAGE_SIZE as u64, true);
    super::set_async_dirty_flush_sg_enabled(false);
    axdriver::set_virtio_async_block_enabled(false);

    assert!(matches!(
        &result,
        Err(super::RangeSyncError::Writeback(
            super::DirtyWritebackError {
                error: VfsError::Io,
                errseq_published: true,
                worker_must_publish: false,
            }
        ))
    ));
    assert_eq!(
        writeback_errors.check_and_advance(&mut cursor),
        Some(VfsError::Io)
    );
    assert_eq!(writeback_errors.check_and_advance(&mut cursor), None);
}

#[test]
fn range_fallback_write_error_is_deferred_to_its_worker_completion() {
    let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x5a, true);
    let writeback_errors = location.writeback_error_state().unwrap();
    let mut cursor = writeback_errors.sample();

    // No SG submission is accepted; the ordinary vectored write is a
    // real range-worker completion and must be published by that worker.
    state.async_write_mode.store(0, Ordering::Release);
    axdriver::set_virtio_async_block_enabled(true);
    super::set_async_dirty_flush_sg_enabled(true);
    let result = cached.sync_range_marked(0, PAGE_SIZE as u64, true);
    super::set_async_dirty_flush_sg_enabled(false);
    axdriver::set_virtio_async_block_enabled(false);

    assert!(matches!(
        &result,
        Err(super::RangeSyncError::Writeback(
            super::DirtyWritebackError {
                error: VfsError::Io,
                errseq_published: false,
                worker_must_publish: true,
            }
        ))
    ));
    assert_eq!(writeback_errors.check_and_advance(&mut cursor), None);

    assert_eq!(cached.complete_range_writeback(result), Err(VfsError::Io));
    assert_eq!(
        writeback_errors.check_and_advance(&mut cursor),
        Some(VfsError::Io)
    );
    assert_eq!(writeback_errors.check_and_advance(&mut cursor), None);
}

#[test]
fn pressure_reclaim_only_drops_clean_unpinned_pages() {
    let (cached, _location, _state) = cached_append_test_file(3 * PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x10, false);
    seed_cached_page(&cached, 1, 0x20, true);
    let pinned_paddr = seed_cached_page(&cached, 2, 0x30, false);
    let pin = cached
        .pin_cached_page_by_paddr(2, pinned_paddr, false)
        .unwrap();

    let mut stats = CachedFileReclaimStats::default();
    assert_eq!(
        reclaim_clean_pages_from_shared(&cached.shared, 16, &mut stats),
        1
    );
    assert!(stats.dirty_pages >= 1);
    assert!(stats.pinned_pages >= 1);
    cached.with_page(0, |page| assert!(page.is_none()));
    cached.with_page(1, |page| assert!(page.is_some()));
    cached.with_page(2, |page| assert!(page.is_some()));
    drop(pin);
}

#[test]
fn fault_reclaim_can_progress_past_a_retained_precise_pin() {
    let (cached, _location, _state) = cached_append_test_file(2 * PAGE_SIZE as u64);
    let pinned = seed_cached_page(&cached, 0, 0x11, false);
    seed_cached_page(&cached, 1, 0x22, false);
    let pin = cached.pin_cached_page_by_paddr(0, pinned, false).unwrap();
    assert_eq!(cached.reclaim_one(), Ok(true));
    assert!(cached.shared.page_cache.lock().contains(&0));
    assert!(!cached.shared.page_cache.lock().contains(&1));
    assert_eq!(cached.reclaim_one(), Ok(false));
    drop(pin);
    assert_eq!(cached.reclaim_one(), Ok(true));
}

#[test]
fn fault_reclaim_retries_when_writeback_is_the_only_resident_page() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x33, false);
    cached
        .shared
        .page_cache
        .lock()
        .get_mut(&0)
        .unwrap()
        .begin_writeback()
        .unwrap();
    assert_eq!(cached.reclaim_one(), Err(VfsError::ResourceBusy));
    cached
        .shared
        .page_cache
        .lock()
        .get_mut(&0)
        .unwrap()
        .end_writeback();
    assert_eq!(cached.reclaim_one(), Ok(true));
}

#[test]
fn pressure_reclaim_records_a_shadow_and_refault_consumes_it() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x33, false);

    let mut stats = CachedFileReclaimStats::default();
    assert_eq!(
        reclaim_clean_pages_from_shared(&cached.shared, 1, &mut stats),
        1
    );
    assert_eq!(cached.cachestat(0, 0).nr_evicted, 1);
    // No later eviction has advanced the nonresident age, so this
    // shadow is recent even when the resident working set is empty.
    assert_eq!(cached.cachestat(0, 0).nr_recently_evicted, 1);

    seed_cached_page(&cached, 0, 0x34, false);
    assert_eq!(cached.cachestat(0, 0).nr_evicted, 0);
    cached.with_page(0, |page| {
        assert!(page.expect("refault must repopulate cache").is_active());
    });
}

#[test]
fn nonresident_age_is_shared_across_inode_generations() {
    let (first, _first_location, _first_state) = cached_append_test_file(PAGE_SIZE as u64);
    let (second, _second_location, _second_state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&first, 0, 0x35, false);
    seed_cached_page(&second, 0, 0x36, false);

    let mut first_stats = CachedFileReclaimStats::default();
    let mut second_stats = CachedFileReclaimStats::default();
    assert_eq!(
        reclaim_clean_pages_from_shared(&first.shared, 1, &mut first_stats),
        1
    );
    let first_age = super::current_file_cache_nonresident_age();
    assert_eq!(
        reclaim_clean_pages_from_shared(&second.shared, 1, &mut second_stats),
        1
    );
    assert!(super::current_file_cache_nonresident_age() > first_age);
    assert_eq!(first.cachestat(0, 0).nr_recently_evicted, 1);
}

#[test]
fn cachestat_zero_resident_window_only_marks_same_age_shadow_recent() {
    assert!(super::file_cache_shadow_is_recent(7, 7, 0));
    assert!(!super::file_cache_shadow_is_recent(8, 7, 0));
}

#[test]
fn cachestat_active_window_excludes_single_touch_cache_pages() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    cached
        .with_page_or_insert(0, |_, evicted| {
            assert!(evicted.is_none());
            Ok(())
        })
        .unwrap();
    {
        let mut cache = cached.shared.page_cache.lock();
        let page = cache.get(&0).unwrap();
        assert!(page.is_referenced());
        assert!(!page.is_active());
    }

    cached.with_page(0, |page| assert!(page.is_some()));
    assert!(cached.shared.page_cache.lock().get(&0).unwrap().is_active());

    // A just-faulted page has no active-window allowance; the second
    // reference promotes it and allows a one-generation refault.
    assert!(!super::file_cache_shadow_is_recent(2, 1, 0));
    assert!(super::file_cache_shadow_is_recent(2, 1, 1));
}

#[test]
fn cachestat_first_write_marks_referenced_without_promotion() {
    init_test_page_allocator();
    let mut page = PageCache::new(false).unwrap();
    page.mark_dirty();
    assert!(page.is_referenced());
    assert!(!page.is_active());

    assert!(page.record_reference());
    page.demote_active();
    assert!(!page.record_reference());
    assert!(page.record_reference());
}

#[test]
fn final_shared_drop_releases_nonempty_resident_cache() {
    init_test_page_allocator();
    let before = super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire);
    let shared = Arc::new(CachedFileShared::new(
        super::CachedFileIdentity {
            device: 91,
            inode: 92,
            object: 93,
        },
        false,
    ));
    let page = PageCache::new(false).unwrap();
    assert!(shared.page_cache.lock().put(0, page).is_none());
    super::file_cache_resident_add(1);

    drop(shared);
    assert_eq!(
        super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire),
        before
    );
}

#[test]
fn invalidation_rollback_restores_resident_accounting() {
    init_test_page_allocator();
    let before = super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire);
    let shared = Arc::new(CachedFileShared::new(
        super::CachedFileIdentity {
            device: 94,
            inode: 95,
            object: 96,
        },
        false,
    ));
    let page = PageCache::new(false).unwrap();
    assert!(shared.page_cache.lock().put(0, page).is_none());
    super::file_cache_resident_add(1);

    let mut transaction = CachedPageInvalidationTransaction::new_shared(shared.clone());
    transaction.stage_all().unwrap();
    assert_eq!(
        super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire),
        before
    );
    drop(transaction);
    assert_eq!(shared.page_cache.lock().len(), 1);
    assert_eq!(
        super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire),
        before + 1
    );

    drop(shared);
    assert_eq!(
        super::FILE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire),
        before
    );
}

#[test]
fn managed_page_baseline_is_stable_during_concurrent_page_allocation() {
    init_test_page_allocator();
    let budget = super::file_cache_shadow_budget();
    let managed = super::FILE_CACHE_MANAGED_PAGES.load(Ordering::Acquire);
    let workers = (0..4)
        .map(|_| {
            thread::spawn(|| {
                for _ in 0..32 {
                    drop(PageCache::new(false).unwrap());
                }
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }

    assert_eq!(
        super::FILE_CACHE_MANAGED_PAGES.load(Ordering::Acquire),
        managed
    );
    assert_eq!(super::file_cache_shadow_budget(), budget);
}

#[test]
fn mmap_cache_access_retries_writeback_without_calling_callbacks() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x37, true);
    let run = super::DirtyWritebackRun {
        page_start: 0,
        bytes: PAGE_SIZE,
        pages: vec![super::DirtyWritebackPage {
            pn: 0,
            data: vec![0x37; PAGE_SIZE],
        }],
    };
    begin_dirty_writeback_run(&cached.shared, &run).unwrap();
    assert!(cached.is_page_cached(0));
    assert!(!cached.is_page_cached(1));
    assert_eq!(
        cached.try_with_page(0, |_| panic!("busy callback")),
        Err::<(), _>(VfsError::ResourceBusy)
    );
    assert_eq!(
        cached.with_page_or_insert_without_reclaim_with_readahead(0, 1, |_| panic!(
            "busy fault callback"
        )),
        Err::<(), _>(VfsError::ResourceBusy)
    );
    assert_eq!(cached.try_with_page(1, |page| page.is_none()), Ok(true));
    finish_dirty_writeback_run(&cached.shared, &run, false);
    assert_eq!(
        cached.try_with_page(0, |page| page.unwrap().is_dirty()),
        Ok(true)
    );
    assert_eq!(
        cached.with_page_or_insert_without_reclaim_with_readahead(0, 1, |page| Ok(page.is_dirty())),
        Ok(true)
    );
}

#[test]
fn fallback_writeback_has_the_same_begin_end_page_state_as_sg() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x37, true);
    let run = super::DirtyWritebackRun {
        page_start: 0,
        bytes: PAGE_SIZE,
        pages: vec![super::DirtyWritebackPage {
            pn: 0,
            data: vec![0x37; PAGE_SIZE],
        }],
    };

    begin_dirty_writeback_run(&cached.shared, &run).unwrap();
    assert!(
        cached
            .shared
            .page_cache
            .lock()
            .get(&0)
            .unwrap()
            .is_writeback()
    );
    finish_dirty_writeback_run(&cached.shared, &run, false);
    cached.with_page(0, |page| {
        let page = page.unwrap();
        assert!(page.is_dirty());
        assert!(!page.is_writeback());
    });

    begin_dirty_writeback_run(&cached.shared, &run).unwrap();
    finish_dirty_writeback_run(&cached.shared, &run, true);
    cached.with_page(0, |page| {
        let page = page.unwrap();
        assert!(!page.is_dirty());
        assert!(!page.is_writeback());
    });
}

#[test]
fn pressure_reclaim_skips_mapping_listeners() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x41, false);
    let owner = CachedFileEvictionOwner::new(77).unwrap();
    let handle = cached
        .add_evict_listener(owner, |_| Err(VfsError::ResourceBusy))
        .unwrap();

    let mut stats = CachedFileReclaimStats::default();
    assert_eq!(
        reclaim_clean_pages_from_shared(&cached.shared, 1, &mut stats),
        0
    );
    assert_eq!(stats.mapped_files, 1);
    cached.with_page(0, |page| assert!(page.is_some()));
    unsafe { cached.remove_evict_listener(handle) };
}

#[test]
fn pressure_reclaim_continues_a_bounded_inode_scan_across_passes() {
    let _epoch_guard = PRESSURE_RECLAIM_EPOCH_TEST_LOCK.lock().unwrap();
    const TEST_SCAN_BUDGET: usize = 2;
    let pages = TEST_SCAN_BUDGET + 1;
    let (cached, _location, _state) = cached_append_test_file((pages * PAGE_SIZE) as u64);
    for page in 0..TEST_SCAN_BUDGET {
        seed_cached_page(&cached, page as u32, 0x61, true);
    }
    seed_cached_page(&cached, TEST_SCAN_BUDGET as u32, 0x62, false);

    let mut first = CachedFileReclaimStats::default();
    assert_eq!(
        reclaim_clean_pages_from_shared_with_scan_budget(
            &cached.shared,
            1,
            TEST_SCAN_BUDGET,
            &mut first,
        ),
        0
    );
    assert_eq!(first.scanned_pages, TEST_SCAN_BUDGET);
    assert_eq!(first.scan_budget_exhausted_files, 1);
    assert_eq!(
        cached
            .shared
            .pressure_reclaim_scan_remaining
            .load(Ordering::Acquire),
        1
    );

    let mut second = CachedFileReclaimStats::default();
    assert_eq!(
        reclaim_clean_pages_from_shared_with_scan_budget(
            &cached.shared,
            1,
            TEST_SCAN_BUDGET,
            &mut second,
        ),
        1
    );
    assert_eq!(second.scanned_pages, 1);
    assert_eq!(second.scan_budget_exhausted_files, 0);
    assert_eq!(
        cached
            .shared
            .pressure_reclaim_scan_remaining
            .load(Ordering::Acquire),
        0
    );
    cached.with_page(TEST_SCAN_BUDGET as u32, |page| assert!(page.is_none()));
}

#[test]
fn pressure_reclaim_does_not_restart_a_completed_inode_scan_epoch() {
    let _epoch_guard = PRESSURE_RECLAIM_EPOCH_TEST_LOCK.lock().unwrap();
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x63, true);

    let mut first = CachedFileReclaimStats::default();
    assert_eq!(
        reclaim_clean_pages_from_shared(&cached.shared, 1, &mut first),
        0
    );
    assert_eq!(first.scanned_pages, 1);
    assert_eq!(first.scan_budget_exhausted_files, 0);

    let mut repeated = CachedFileReclaimStats::default();
    assert_eq!(
        reclaim_clean_pages_from_shared(&cached.shared, 1, &mut repeated),
        0
    );
    assert_eq!(repeated.scanned_pages, 0);
    assert_eq!(repeated.scan_budget_exhausted_files, 0);
}

#[test]
fn pressure_reclaim_new_epoch_reenables_a_completed_inode_scan() {
    let _epoch_guard = PRESSURE_RECLAIM_EPOCH_TEST_LOCK.lock().unwrap();
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x64, true);

    let mut completed = CachedFileReclaimStats::default();
    assert_eq!(
        reclaim_clean_pages_from_shared(&cached.shared, 1, &mut completed),
        0
    );
    assert_eq!(completed.scanned_pages, 1);

    advance_clean_cached_file_reclaim_scan_epoch();
    let mut next_epoch = CachedFileReclaimStats::default();
    assert_eq!(
        reclaim_clean_pages_from_shared(&cached.shared, 1, &mut next_epoch),
        0
    );
    assert_eq!(next_epoch.scanned_pages, 1);
}

#[test]
fn pressure_reconcile_does_not_apply_an_old_delta_to_new_retention() {
    let (cached, location, _state) = cached_append_test_file(2 * PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x51, false);
    seed_cached_page(&cached, 1, 0x52, true);

    // Model the critical old interleaving directly: reclaim has already
    // removed one clean page, then a last-close transaction publishes a
    // retention count based on the one dirty page that remains.  The
    // reconciliation must preserve that actual count rather than subtract
    // the earlier reclaim delta from it.
    let clean = cached.shared.page_cache.lock().pop(&0).unwrap();
    drop(clean);
    let key = cached_file_registry_key(&location);
    {
        let mut registry = file_cache_registry().lock();
        let entry = registry
            .entry(key)
            .or_insert_with(|| FileUserData::new(&location, &cached.shared));
        let retired = entry.retain_closed(&location, &cached.shared, 1);
        drop(retired);
    }

    synchronize_retained_page_count(&cached.shared, 1);
    {
        let registry = file_cache_registry().lock();
        let entry = registry.get(&key).unwrap();
        assert_eq!(entry.retained_pages, 1);
        assert!(
            entry
                .retained
                .as_ref()
                .is_some_and(|retained| Arc::ptr_eq(retained, &cached.shared))
        );
    }
    cached.with_page(1, |page| assert!(page.is_some_and(|page| page.is_dirty())));

    let retired = file_cache_registry().lock().remove(&key);
    drop(retired);
    cached.shared.unlinked.store(true, Ordering::Release);
}

struct RawPhysicalReader {
    paddr: usize,
    remaining: usize,
}

impl RawPhysicalReader {
    fn new(paddr: usize, len: usize) -> Self {
        Self {
            paddr,
            remaining: len,
        }
    }
}

impl Read for RawPhysicalReader {
    fn read(&mut self, buf: &mut [u8]) -> axio::Result<usize> {
        let len = self.remaining.min(buf.len());
        let src = physical_to_virtual(axhal::mem::PhysAddr::from(self.paddr)).as_ptr();
        unsafe { core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), len) };
        self.paddr += len;
        self.remaining -= len;
        Ok(len)
    }
}

impl IoBuf for RawPhysicalReader {
    fn remaining(&self) -> usize {
        self.remaining
    }
}

struct RawPhysicalWriter {
    paddr: usize,
    remaining: usize,
}

impl RawPhysicalWriter {
    fn new(paddr: usize, len: usize) -> Self {
        Self {
            paddr,
            remaining: len,
        }
    }
}

impl Write for RawPhysicalWriter {
    fn write(&mut self, buf: &[u8]) -> axio::Result<usize> {
        let len = self.remaining.min(buf.len());
        let dst = physical_to_virtual(axhal::mem::PhysAddr::from(self.paddr)).as_mut_ptr();
        unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), dst, len) };
        self.paddr += len;
        self.remaining -= len;
        Ok(len)
    }

    fn flush(&mut self) -> axio::Result<()> {
        Ok(())
    }
}

impl IoBufMut for RawPhysicalWriter {
    fn remaining_mut(&self) -> usize {
        self.remaining
    }
}

struct FaultingReader {
    bytes: Vec<u8>,
    position: usize,
    calls: usize,
    fail_call: usize,
}

impl FaultingReader {
    fn new(len: usize, fail_call: usize) -> Self {
        Self {
            bytes: vec![0x5a; len],
            position: 0,
            calls: 0,
            fail_call,
        }
    }
}

impl Read for FaultingReader {
    fn read(&mut self, dst: &mut [u8]) -> axio::Result<usize> {
        let call = self.calls;
        self.calls += 1;
        if call == self.fail_call {
            return Err(axio::Error::BadAddress);
        }
        let len = dst.len().min(self.remaining());
        dst[..len].copy_from_slice(&self.bytes[self.position..self.position + len]);
        self.position += len;
        Ok(len)
    }
}

impl IoBuf for FaultingReader {
    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }
}

#[test]
fn pinned_same_cache_page_read_and_scatter_read_use_overlap_bounce() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    let paddr: usize = seed_cached_page(&cached, 0, 0, false).into();
    let expected = (0..96).map(|value| value as u8).collect::<Vec<_>>();
    cached.with_page(0, |page| {
        page.unwrap().data()[..expected.len()].copy_from_slice(&expected);
    });

    let scalar = [PinnedPhysicalSegment::new(paddr + 512, 32)];
    assert_eq!(
        unsafe { cached.read_at_pinned_segments(&scalar, 0, false) },
        Ok(32)
    );
    let scatter = [
        PinnedPhysicalSegment::new(paddr + 1024, 32),
        PinnedPhysicalSegment::new(paddr + 1536, 32),
    ];
    assert_eq!(
        unsafe { cached.read_at_pinned_segments(&scatter, 32, false) },
        Ok(64)
    );

    cached.with_page(0, |page| {
        let data = page.unwrap().data();
        assert_eq!(&data[512..544], &expected[..32]);
        assert_eq!(&data[1024..1056], &expected[32..64]);
        assert_eq!(&data[1536..1568], &expected[64..96]);
    });
}

#[test]
fn mutable_pinned_segment_validation_is_fixed_and_bounded() {
    let segments: [PinnedPhysicalSegment; MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS] =
        core::array::from_fn(|index| PinnedPhysicalSegment::new(0x1000 + index * 0x1000, 0x1000));
    assert_eq!(
        validate_pinned_physical_segments(&segments, true),
        Ok(MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS * 0x1000)
    );

    let overflow: [PinnedPhysicalSegment; MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS + 1] =
        core::array::from_fn(|index| PinnedPhysicalSegment::new(0x1000 + index * 0x1000, 0x1000));
    assert_eq!(
        validate_pinned_physical_segments(&overflow, true),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(
        validate_pinned_physical_segments(
            &[
                PinnedPhysicalSegment::new(0x1000, 0x1000),
                PinnedPhysicalSegment::new(0x1800, 0x1000),
            ],
            true,
        ),
        Err(VfsError::InvalidInput)
    );

    let bounce = try_zeroed_pinned_io_bounce(PAGE_SIZE).unwrap();
    assert_eq!(bounce.len(), PAGE_SIZE);
    assert!(bounce.iter().all(|byte| *byte == 0));
}

#[test]
fn pinned_same_cache_page_write_and_scatter_write_use_overlap_bounce() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    let paddr: usize = seed_cached_page(&cached, 0, 0, false).into();
    cached.with_page(0, |page| {
        let data = page.unwrap().data();
        data[512..544].fill(0x51);
        data[1024..1040].fill(0x61);
        data[1536..1552].fill(0x71);
    });

    let scalar = [PinnedPhysicalSegment::new(paddr + 512, 32)];
    assert_eq!(
        unsafe { cached.write_at_pinned_segments(&scalar, 0, false) },
        Ok(32)
    );
    let scatter = [
        PinnedPhysicalSegment::new(paddr + 1024, 16),
        PinnedPhysicalSegment::new(paddr + 1536, 16),
    ];
    assert_eq!(
        unsafe { cached.write_at_pinned_segments(&scatter, 128, false) },
        Ok(32)
    );

    cached.with_page(0, |page| {
        let data = page.unwrap().data();
        assert!(data[..32].iter().all(|byte| *byte == 0x51));
        assert!(data[128..144].iter().all(|byte| *byte == 0x61));
        assert!(data[144..160].iter().all(|byte| *byte == 0x71));
    });
}

#[test]
fn aligned_same_cache_page_pin_falls_back_from_direct_bypass() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    let paddr = seed_cached_page(&cached, 0, 0x5a, false);
    let segment = [PinnedPhysicalSegment::new(paddr.into(), PAGE_SIZE)];
    let pin = cached.pin_cached_page_by_paddr(0, paddr, true).unwrap();

    assert_eq!(
        unsafe { cached.read_at_pinned_segments(&segment, 0, true) },
        Ok(PAGE_SIZE)
    );
    assert_eq!(
        unsafe { cached.write_at_pinned_segments(&segment, 0, true) },
        Ok(PAGE_SIZE)
    );
    drop(pin);
    cached.with_page(0, |page| {
        let page = page.unwrap();
        assert!(page.is_dirty());
        assert!(page.data().iter().all(|byte| *byte == 0x5a));
    });
}

#[test]
fn filesystem_inode_aliases_share_pinned_overlap_policy() {
    init_test_page_allocator();
    let state = AppendTestState::new(PAGE_SIZE as u64);
    let fs = Filesystem::new(RegistryTestFs::new_for_append(NodeFlags::empty(), state));
    // Distinct locations for the same filesystem/inode model two hardlink
    // dentries and must resolve to one cache registry owner.
    let first = CachedFile::get_or_create(Mountpoint::new_root(&fs).root_location());
    let alias = CachedFile::get_or_create(Mountpoint::new_root(&fs).root_location());
    assert!(first.ptr_eq(&alias));

    let paddr: usize = seed_cached_page(&first, 0, 0x2a, false).into();
    let destination = [PinnedPhysicalSegment::new(paddr + 512, 32)];
    assert_eq!(
        unsafe { alias.read_at_pinned_segments(&destination, 0, false) },
        Ok(32)
    );
    first.with_page(0, |page| {
        assert!(
            page.unwrap().data()[512..544]
                .iter()
                .all(|byte| *byte == 0x2a)
        );
    });
}

#[test]
fn generic_raw_physical_io_bounces_outside_page_data_borrow() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    let paddr: usize = seed_cached_page(&cached, 0, 0, false).into();
    cached.with_page(0, |page| {
        let data = page.unwrap().data();
        data[..32].fill(0x31);
        data[1024..1056].fill(0x41);
    });

    assert_eq!(
        cached.read_at(RawPhysicalWriter::new(paddr + 512, 32), 0),
        Ok(32)
    );
    assert_eq!(
        cached.write_at(RawPhysicalReader::new(paddr + 1024, 32), 128),
        Ok(32)
    );
    cached.with_page(0, |page| {
        let data = page.unwrap().data();
        assert!(data[512..544].iter().all(|byte| *byte == 0x31));
        assert!(data[128..160].iter().all(|byte| *byte == 0x41));
    });
}

#[test]
fn transactional_sync_read_never_calls_async_lower_hook() {
    let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
    state.full_page_io.store(true, Ordering::Release);
    let mut output = vec![0xa5; PAGE_SIZE];

    assert_eq!(cached.read_at_sync(&mut &mut output[..], 0), Ok(PAGE_SIZE));
    assert_eq!(state.async_read_calls.load(Ordering::Acquire), 0);
    assert_eq!(state.read_calls.load(Ordering::Acquire), 1);
    assert!(output.iter().all(|byte| *byte == 0));
}

#[cfg(feature = "ext4")]
#[test]
fn pinned_fallback_policy_never_calls_async_lower_hook() {
    struct AsyncMappedReadReset;

    impl Drop for AsyncMappedReadReset {
        fn drop(&mut self) {
            lwext4_rust::set_async_mapped_read_enabled(false);
        }
    }

    lwext4_rust::set_async_mapped_read_enabled(true);
    let _reset = AsyncMappedReadReset;

    let (reader, _location, read_state) = cached_append_test_file(PAGE_SIZE as u64);
    read_state.full_page_io.store(true, Ordering::Release);
    let reader = FileBackend::Cached(reader);
    let (destination_cache, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    let destination_paddr = seed_cached_page(&destination_cache, 0, 0xa5, false);
    let destination_pin = destination_cache
        .pin_cached_page_by_paddr(0, destination_paddr, true)
        .unwrap();
    let destination = [PinnedPhysicalSegment::new(
        usize::from(destination_paddr) + 128,
        8,
    )];
    assert_eq!(
        unsafe { reader.read_at_pinned_segments(&destination, 1, true) },
        Ok(8)
    );
    assert_eq!(read_state.async_read_calls.load(Ordering::Acquire), 0);
    assert_eq!(read_state.read_calls.load(Ordering::Acquire), 1);
    destination_cache.with_page(0, |page| {
        assert!(page.unwrap().data()[128..136].iter().all(|byte| *byte == 0));
    });
    drop(destination_pin);

    let (writer, _location, write_state) = cached_append_test_file(PAGE_SIZE as u64);
    write_state.full_page_io.store(true, Ordering::Release);
    let cached_writer = writer.clone();
    let writer = FileBackend::Cached(writer);
    let (source_cache, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    let source_paddr = seed_cached_page(&source_cache, 0, 0x5a, false);
    let source_pin = source_cache
        .pin_cached_page_by_paddr(0, source_paddr, false)
        .unwrap();
    let source = [PinnedPhysicalSegment::new(
        usize::from(source_paddr) + 128,
        8,
    )];
    assert_eq!(
        unsafe { writer.write_at_pinned_segments(&source, 1, true) },
        Ok(8)
    );
    assert_eq!(write_state.async_read_calls.load(Ordering::Acquire), 0);
    assert_eq!(write_state.read_calls.load(Ordering::Acquire), 1);
    cached_writer.with_page(0, |page| {
        assert!(page.unwrap().data()[1..9].iter().all(|byte| *byte == 0x5a));
    });
    drop(source_pin);
}

#[test]
fn deferred_open_leaves_truncate_for_the_prepared_consumer_to_commit() {
    let state = AppendTestState::new(41);
    let fs = Filesystem::new(RegistryTestFs::new_for_append(
        NodeFlags::NON_CACHEABLE,
        state.clone(),
    ));
    let location = Mountpoint::new_root(&fs).root_location();
    let mut options = OpenOptions::new();
    options.write(true).truncate(true).direct(true);

    let file = options
        .open_loc_deferred_truncate(location)
        .unwrap()
        .into_file()
        .unwrap();
    assert_eq!(state.inode_len.load(Ordering::Acquire), 41);

    file.commit_deferred_open_truncate().unwrap();
    assert_eq!(state.inode_len.load(Ordering::Acquire), 0);
}

#[test]
fn concurrent_start_deferred_open_truncate_is_ofd_private_and_commits_once() {
    let state = AppendTestState::new(41);
    let fs = Filesystem::new(RegistryTestFs::new_for_append(
        NodeFlags::NON_CACHEABLE,
        state.clone(),
    ));
    let location = Mountpoint::new_root(&fs).root_location();
    let mut options = OpenOptions::new();
    options.write(true).truncate(true).direct(true);
    let deferred = Arc::new(
        options
            .open_loc_deferred_truncate(location.clone())
            .unwrap()
            .into_file()
            .unwrap(),
    );
    let ordinary = File::new(FileBackend::Direct(location), FileFlags::WRITE);
    assert_eq!(
        ordinary.commit_deferred_open_truncate(),
        Err(VfsError::InvalidInput)
    );

    let start = Arc::new(Barrier::new(3));
    let mut commits = Vec::new();
    for _ in 0..2 {
        let deferred = deferred.clone();
        let start = start.clone();
        commits.push(thread::spawn(move || {
            start.wait();
            deferred.commit_deferred_open_truncate()
        }));
    }
    start.wait();
    let results = commits
        .into_iter()
        .map(|commit| commit.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Err(VfsError::InvalidInput))
            .count(),
        1
    );
    assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
    assert_eq!(state.inode_len.load(Ordering::Acquire), 0);
}

#[test]
fn cached_truncate_rejects_pin_preparation_before_inode_mutation() {
    let state = AppendTestState::new(41);
    let fs = Filesystem::new(RegistryTestFs::new_for_append(
        NodeFlags::empty(),
        state.clone(),
    ));
    let mountpoint = Mountpoint::new_root(&fs);
    let cached = CachedFile::get_or_create(mountpoint.root_location());
    let window = cached.begin_user_io_pin_window().unwrap();

    assert!(matches!(cached.set_len(0), Err(VfsError::ResourceBusy)));
    assert_eq!(state.inode_len.load(Ordering::Acquire), 41);

    drop(window);
    cached.set_len(0).unwrap();
    assert_eq!(state.inode_len.load(Ordering::Acquire), 0);
}

#[test]
fn concurrent_buffered_cache_users_share_admission() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    let cached = Arc::new(cached);
    let (holding_tx, holding_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    let first_cached = cached.clone();
    let first = thread::spawn(move || {
        first_cached.with_page_or_insert(0, |_, _| {
            holding_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
    });
    holding_rx.recv().unwrap();

    let (started_tx, started_rx) = mpsc::channel();
    let second_cached = cached.clone();
    let second = thread::spawn(move || {
        started_tx.send(()).unwrap();
        second_cached.with_page_or_insert(0, |_, _| Ok(()))
    });
    started_rx.recv().unwrap();

    let mut admitted = false;
    for _ in 0..10_000 {
        if cached.shared.user_io_pin_admission.lock().cache_users == 2 {
            admitted = true;
            break;
        }
        thread::yield_now();
    }
    assert!(admitted, "second buffered cache user was not admitted");

    release_tx.send(()).unwrap();
    assert_eq!(first.join().unwrap(), Ok(()));
    assert_eq!(second.join().unwrap(), Ok(()));
    assert_eq!(cached.shared.user_io_pin_admission.lock().cache_users, 0);
}

#[test]
fn prepared_listener_is_released_without_commit_on_transaction_abort() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x41, false);
    let owner = CachedFileEvictionOwner::new(1).unwrap();
    let handle = cached
        .add_evict_listener(owner, |_| prepared_listener())
        .unwrap();
    let mutation = cached.begin_cache_invalidating_mutation().unwrap();
    let mut transaction = CachedPageInvalidationTransaction::new(&mutation);
    transaction.stage_all().unwrap();
    transaction.prepare_evictions().unwrap();
    drop(transaction);
    assert_eq!(cached.cachestat(0, 0).nr_cache, 1);

    unsafe { cached.remove_evict_listener(handle) };
}

#[test]
fn repeated_shared_page_writes_rearm_listener_on_each_sync() {
    let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
    state.full_page_io.store(true, Ordering::Release);
    seed_cached_page(&cached, 0, b'A', true);
    let listener_calls = Arc::new(AtomicUsize::new(0));
    let calls = listener_calls.clone();
    let owner = CachedFileEvictionOwner::new(3).unwrap();
    let handle = cached
        .add_evict_listener(owner, move |_| {
            calls.fetch_add(1, Ordering::AcqRel);
            prepared_listener()
        })
        .unwrap();

    cached.sync(false).unwrap();
    assert_eq!(listener_calls.load(Ordering::Acquire), 1);
    assert_eq!(state.stored_first_byte.load(Ordering::Acquire), b'A');
    cached.with_page(0, |page| assert!(!page.unwrap().is_dirty()));

    cached.with_page(0, |page| {
        let page = page.unwrap();
        page.data()[0] = b'B';
        page.mark_dirty();
    });
    cached.sync(false).unwrap();
    assert_eq!(listener_calls.load(Ordering::Acquire), 2);
    assert_eq!(state.stored_first_byte.load(Ordering::Acquire), b'B');
    cached.with_page(0, |page| assert!(!page.unwrap().is_dirty()));

    unsafe { cached.remove_evict_listener(handle) };
}

#[test]
fn foreign_deferred_listener_rolls_back_dirty_invalidation() {
    let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
    let original_paddr = seed_cached_page(&cached, 0, 0x5a, true);
    let foreign = CachedFileEvictionOwner::new(2).unwrap();
    let handle = cached
        .add_evict_listener(foreign, |_| Err(VfsError::ResourceBusy))
        .unwrap();

    let mutation = cached.begin_cache_invalidating_mutation().unwrap();
    let result = {
        let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
        invalidation
            .stage_all()
            .and_then(|()| invalidation.prepare_evictions())
    };
    assert_eq!(result, Err(VfsError::ResourceBusy));
    drop(mutation);
    cached.with_page(0, |page| {
        let page = page.expect("listener contention must restore the staged page");
        assert_eq!(page.paddr(), original_paddr);
        assert!(page.is_dirty());
        assert!(page.data().iter().all(|byte| *byte == 0x5a));
    });
    assert!(state.write_offsets.lock().is_empty());
    assert_eq!(state.set_len_calls.load(Ordering::Acquire), 0);

    unsafe { cached.remove_evict_listener(handle) };
}

#[test]
fn short_invalidation_writeback_restores_the_dirty_page() {
    let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
    let original_paddr = seed_cached_page(&cached, 0, 0x5a, true);

    let mutation = cached.begin_cache_invalidating_mutation().unwrap();
    let result = {
        let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
        invalidation.stage_all().unwrap();
        invalidation.writeback(location.entry().as_file().unwrap(), false)
    };
    assert_eq!(result, Err(VfsError::Io));
    drop(mutation);
    cached.with_page(0, |page| {
        let page = page.expect("short writeback must restore the staged page");
        assert_eq!(page.paddr(), original_paddr);
        assert!(page.is_dirty());
        assert!(page.data().iter().all(|byte| *byte == 0x5a));
    });
    assert_eq!(state.write_calls.load(Ordering::Acquire), 1);
}

#[test]
fn precise_pin_blocks_cached_and_direct_truncate_before_raw_set_len() {
    for direct in [false, true] {
        let (cached, location, state) = cached_append_test_file((PAGE_SIZE * 2) as u64);
        let paddr = seed_cached_page(&cached, 1, 0x33, false);
        let pin = cached.pin_cached_page_by_paddr(1, paddr, false).unwrap();

        let result = if direct {
            FileBackend::Direct(location).set_len(0)
        } else {
            cached.set_len(0)
        };
        assert_eq!(result, Err(VfsError::ResourceBusy));
        assert_eq!(state.set_len_calls.load(Ordering::Acquire), 0);
        assert_eq!(
            state.inode_len.load(Ordering::Acquire),
            (PAGE_SIZE * 2) as u64
        );
        drop(pin);
    }
}

#[test]
fn non_overlapping_precise_pin_allows_cached_shrink() {
    let (cached, _location, state) = cached_append_test_file((PAGE_SIZE * 3) as u64);
    let paddr = seed_cached_page(&cached, 0, 0x11, false);
    seed_cached_page(&cached, 2, 0x22, false);
    let pin = cached.pin_cached_page_by_paddr(0, paddr, false).unwrap();

    cached.set_len((PAGE_SIZE * 2) as u64).unwrap();
    assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        state.inode_len.load(Ordering::Acquire),
        (PAGE_SIZE * 2) as u64
    );
    cached.with_page(0, |page| assert!(page.is_some()));
    cached.with_page(2, |page| assert!(page.is_none()));
    drop(pin);
}

#[test]
fn cached_shrink_still_excludes_direct_io_on_preserved_bytes() {
    let (cached, _location, state) = cached_append_test_file(3 * PAGE_SIZE as u64);
    for kind in [
        RangeCacheLeaseKind::DirectRead,
        RangeCacheLeaseKind::DirectWrite,
    ] {
        let lease =
            CachedFileShared::try_range_cache_lease(&cached.shared, 0..PAGE_SIZE as u64, kind)
                .unwrap();
        assert_eq!(
            cached.set_len(2 * PAGE_SIZE as u64),
            Err(VfsError::ResourceBusy)
        );
        assert_eq!(state.set_len_calls.load(Ordering::Acquire), 0);
        assert!(lease.revalidate());
        drop(lease);
    }
    cached.set_len(2 * PAGE_SIZE as u64).unwrap();
    assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
}

#[test]
fn write_pin_marks_page_dirty_and_retains_writeback_ownership_on_release() {
    let (cached, location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    let paddr = seed_cached_page(&cached, 0, 0x44, false);
    let window = cached.begin_user_io_pin_window().unwrap();
    let pin = cached.pin_cached_page_by_paddr(0, paddr, true).unwrap();
    drop(window);

    cached.with_page(0, |page| assert!(!page.unwrap().is_dirty()));
    drop(pin);
    cached.with_page(0, |page| assert!(page.unwrap().is_dirty()));
    let key = cached_file_registry_key(&location);
    assert!(
        file_cache_registry()
            .lock()
            .get(&key)
            .is_some_and(|entry| entry.writeback_anchor.is_some())
    );
}

#[test]
fn direct_truncate_failure_restores_staged_dirty_page() {
    let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
    let original_paddr = seed_cached_page(&cached, 0, 0x6b, true);
    state.full_page_io.store(true, Ordering::Release);
    state.fail_set_len.store(true, Ordering::Release);

    assert_eq!(
        FileBackend::Direct(location).set_len(0),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
    assert_eq!(state.inode_len.load(Ordering::Acquire), PAGE_SIZE as u64);
    assert_eq!(&*state.write_offsets.lock(), &[0]);
    cached.with_page(0, |page| {
        let page = page.expect("failed truncate must restore the staged page");
        assert_eq!(page.paddr(), original_paddr);
        assert!(page.is_dirty());
        assert!(page.data().iter().all(|byte| *byte == 0x6b));
    });
}

#[test]
fn cached_atomic_truncate_failure_restores_written_back_dirty_page() {
    let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
    let original_paddr = seed_cached_page(&cached, 0, 0x6d, true);
    state.full_page_io.store(true, Ordering::Release);
    state.fail_set_len.store(true, Ordering::Release);

    assert_eq!(cached.set_len(0), Err(VfsError::InvalidInput));
    assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
    assert_eq!(state.inode_len.load(Ordering::Acquire), PAGE_SIZE as u64);
    assert_eq!(&*state.write_offsets.lock(), &[0]);
    cached.with_page(0, |page| {
        let page = page.expect("atomic truncate failure must restore the staged page");
        assert_eq!(page.paddr(), original_paddr);
        assert!(page.is_dirty());
        assert!(page.data().iter().all(|byte| *byte == 0x6d));
    });
}

#[test]
fn non_atomic_truncate_failure_discards_potentially_stale_cache() {
    let (cached, location, state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x71, true);
    state.full_page_io.store(true, Ordering::Release);
    state.fail_set_len.store(true, Ordering::Release);
    state.set_len_failure_atomic.store(false, Ordering::Release);

    assert_eq!(
        FileBackend::Direct(location).set_len(0),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
    assert_eq!(state.inode_len.load(Ordering::Acquire), 0);
    cached.with_page(0, |page| assert!(page.is_none()));
}

#[test]
fn cached_non_atomic_truncate_failure_writes_back_before_discard() {
    let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x73, true);
    state.full_page_io.store(true, Ordering::Release);
    state.fail_set_len.store(true, Ordering::Release);
    state.set_len_failure_atomic.store(false, Ordering::Release);

    assert_eq!(cached.set_len(0), Err(VfsError::InvalidInput));
    assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
    assert_eq!(state.inode_len.load(Ordering::Acquire), 0);
    assert_eq!(&*state.write_offsets.lock(), &[0]);
    cached.with_page(0, |page| assert!(page.is_none()));
}

#[test]
fn partial_page_truncate_zeroes_tail_and_discards_full_pages() {
    let (cached, _location, state) = cached_append_test_file((PAGE_SIZE * 3) as u64);
    seed_cached_page(&cached, 1, 0x7c, false);
    seed_cached_page(&cached, 2, 0x2c, false);
    let new_len = (PAGE_SIZE + 17) as u64;

    cached.set_len(new_len).unwrap();
    assert_eq!(state.inode_len.load(Ordering::Acquire), new_len);
    cached.with_page(1, |page| {
        let page = page.unwrap();
        assert!(page.data()[..17].iter().all(|byte| *byte == 0x7c));
        assert!(page.data()[17..].iter().all(|byte| *byte == 0));
        assert!(page.is_dirty());
    });
    cached.with_page(2, |page| assert!(page.is_none()));
}

#[test]
fn direct_invalidation_holds_pin_admission_through_lower_operation() {
    let (cached, location, _state) = cached_append_test_file(0);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    let operation = thread::spawn(move || {
        with_sync_and_invalidate_cached_file_pages(&location, || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
    });
    entered_rx.recv().unwrap();
    assert!(matches!(
        cached.begin_user_io_pin_window(),
        Err(VfsError::ResourceBusy)
    ));
    release_tx.send(()).unwrap();
    assert_eq!(operation.join().unwrap(), Ok(()));
    drop(cached.begin_user_io_pin_window().unwrap());
}

#[test]
fn aligned_write_bypass_waits_for_existing_writeback_reader() {
    let (cached, _location, state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x52, true);
    state.full_page_io.store(true, Ordering::Release);

    let observer = cached.clone();
    let writeback_reader = observer.shared.writeback_lock.read();
    let (started_tx, started_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let new_page = vec![0x6a; PAGE_SIZE];
    let writer = thread::spawn(move || {
        started_tx.send(()).unwrap();
        result_tx.send(cached.write_at_slice(&new_page, 0)).unwrap();
    });
    started_rx.recv().unwrap();

    let mut owns_direct_io = false;
    for _ in 0..10_000 {
        if observer.shared.direct_io_lock.try_lock().is_none() {
            owns_direct_io = true;
            break;
        }
        thread::yield_now();
    }
    assert!(
        owns_direct_io,
        "aligned bypass did not enter its direct-I/O domain"
    );
    assert_eq!(
        result_rx.recv_timeout(Duration::from_millis(20)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    assert_eq!(state.write_calls.load(Ordering::Acquire), 0);
    observer.with_page(0, |page| {
        let page = page.expect("blocked bypass must not stage the dirty cache page");
        assert!(page.is_dirty());
        assert!(page.data().iter().all(|byte| *byte == 0x52));
    });

    drop(writeback_reader);
    assert_eq!(
        result_rx.recv_timeout(Duration::from_secs(1)),
        Ok(Ok(PAGE_SIZE))
    );
    writer.join().unwrap();
    assert_eq!(state.write_calls.load(Ordering::Acquire), 2);
    assert_eq!(state.stored_first_byte.load(Ordering::Acquire), 0x6a);
    observer.with_page(0, |page| assert!(page.is_none()));
}

#[test]
fn discard_waits_for_in_flight_writeback_before_staging_pages() {
    let (cached, _location, _state) = cached_append_test_file(PAGE_SIZE as u64);
    seed_cached_page(&cached, 0, 0x52, true);
    let shared = cached.shared.clone();
    let (writeback_started_tx, writeback_started_rx) = mpsc::channel();
    let (release_writeback_tx, release_writeback_rx) = mpsc::channel();

    let writeback_shared = shared.clone();
    let writeback = thread::spawn(move || {
        let _writeback_guard = writeback_shared.writeback_lock.read();
        writeback_shared
            .page_cache
            .lock()
            .get_mut(&0)
            .unwrap()
            .begin_writeback()
            .unwrap();
        writeback_started_tx.send(()).unwrap();
        release_writeback_rx.recv().unwrap();
        writeback_shared
            .page_cache
            .lock()
            .get_mut(&0)
            .unwrap()
            .end_writeback();
    });
    writeback_started_rx.recv().unwrap();

    let (discard_result_tx, discard_result_rx) = mpsc::channel();
    let discard = thread::spawn(move || {
        discard_result_tx
            .send(discard_cached_pages(&shared))
            .unwrap();
    });
    assert_eq!(
        discard_result_rx.recv_timeout(Duration::from_millis(20)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );

    release_writeback_tx.send(()).unwrap();
    writeback.join().unwrap();
    assert_eq!(
        discard_result_rx.recv_timeout(Duration::from_secs(1)),
        Ok(Ok(()))
    );
    discard.join().unwrap();
    cached.with_page(0, |page| assert!(page.is_none()));
}

#[test]
fn path_only_open_skips_the_filesystem_open_callback() {
    let state = AppendTestState::new(0);
    let fs = Filesystem::new(RegistryTestFs::new_for_append(
        NodeFlags::NON_CACHEABLE,
        state.clone(),
    ));
    let location = Mountpoint::new_root(&fs).root_location();
    let mut options = OpenOptions::new();
    options.path(true);

    let result = options.open_loc(location).unwrap().into_file().unwrap();
    assert!(result.is_path());
    assert_eq!(state.open_calls.load(Ordering::Acquire), 0);
}

fn assert_positioned_append_offsets(state: &AppendTestState) {
    assert_eq!(&*state.write_offsets.lock(), &[0, 2]);
    assert_eq!(state.append_calls.load(Ordering::Acquire), 0);
}

#[test]
fn explicit_current_ignores_default_append_for_all_write_forms() {
    let (file, state) = append_test_file(NodeFlags::NON_CACHEABLE, 41);
    let mut src = Cursor::new(&b"abcd"[..]);
    assert_eq!(
        file.write_with_placement(&mut src, WritePlacement::Current),
        Ok(2)
    );
    assert_eq!(&*state.write_offsets.lock(), &[0]);
    assert_eq!(state.append_calls.load(Ordering::Acquire), 0);

    let (file, state) = append_test_file(NodeFlags::NON_CACHEABLE, 41);
    assert_eq!(
        file.write_slice_with_placement(b"abcd", WritePlacement::Current),
        Ok(2)
    );
    assert_eq!(&*state.write_offsets.lock(), &[0]);
    assert_eq!(state.append_calls.load(Ordering::Acquire), 0);

    let (file, state) = append_test_file(NodeFlags::NON_CACHEABLE, 41);
    assert_eq!(
        file.write_vectored_slice_with_placement(&[b"abcd", b"ef"], WritePlacement::Current,),
        Ok(2)
    );
    assert_eq!(&*state.write_offsets.lock(), &[0]);
    assert_eq!(state.append_calls.load(Ordering::Acquire), 0);
}

#[test]
fn current_position_transfer_commits_only_destination_prefix() {
    let (file, state) = append_test_file_with_access(NodeFlags::NON_CACHEABLE, 8, FileFlags::READ);
    let mut buf = [0u8; 4];

    assert_eq!(
        file.read_slice_then(&mut buf, |data| {
            assert_eq!(data, b"abcd");
            Ok(2)
        }),
        Ok(2)
    );
    assert_eq!(
        file.read_slice_then(&mut buf, |data| {
            assert_eq!(data, b"cdef");
            Err(VfsError::InvalidInput)
        }),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(
        file.read_slice_then(&mut buf, |data| {
            assert_eq!(data, b"cdef");
            Ok(data.len())
        }),
        Ok(4)
    );
    assert_eq!(file.read_slice(&mut buf), Ok(2));
    assert_eq!(&buf[..2], b"gh");
    assert_eq!(&*state.read_offsets.lock(), &[0, 2, 2, 6]);
}

#[test]
fn checked_current_read_rejects_before_backend_io_and_cursor_commit() {
    let (file, state) = append_test_file_with_access(NodeFlags::NON_CACHEABLE, 8, FileFlags::READ);
    let mut buf = [0u8; 4];

    assert_eq!(
        file.read_slice_at_current_checked_then(
            &mut buf,
            |offset| {
                assert_eq!(offset, 0);
                Err(VfsError::PermissionDenied)
            },
            |_data, _offset| unreachable!(),
        ),
        Err(VfsError::PermissionDenied)
    );
    assert!(state.read_offsets.lock().is_empty());
    assert_eq!(file.read_slice(&mut buf), Ok(4));
    assert_eq!(&buf, b"abcd");
    assert_eq!(&*state.read_offsets.lock(), &[0]);
}

#[test]
fn current_write_callback_uses_frozen_offset_and_commits_only_its_prefix() {
    let (file, state) = append_test_file_with_access(NodeFlags::NON_CACHEABLE, 8, FileFlags::WRITE);

    assert_eq!(
        file.with_current_position(|offset| {
            assert_eq!(offset, 0);
            Ok(offset)
        }),
        Ok(0)
    );

    assert_eq!(
        file.write_slice_at_current_then(b"abcd", |data, offset| {
            assert_eq!(offset, 0);
            file.write_at_slice(data, offset)
        }),
        Ok(2)
    );
    assert_eq!(
        file.write_slice_at_current_then(b"ef", |_data, offset| {
            assert_eq!(offset, 2);
            Err(VfsError::PermissionDenied)
        }),
        Err(VfsError::PermissionDenied)
    );
    let mut handle = &file;
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(2));
    assert_eq!(&*state.write_offsets.lock(), &[0]);
}

#[test]
fn operation_position_transaction_commits_once_and_rolls_back_errors() {
    let (file, _state) = append_test_file_with_access(
        NodeFlags::NON_CACHEABLE,
        16,
        FileFlags::READ | FileFlags::WRITE,
    );

    assert_eq!(
        file.with_current_position_transaction(8, |offset| {
            assert_eq!(offset, 0);
            // Model two internal chunks without publishing the intermediate
            // cursor to lseek/read/write users of this description.
            Ok((17, 6))
        }),
        Ok(17)
    );
    let mut handle = &file;
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(6));

    assert_eq!(
        file.with_current_position_transaction(4, |offset| {
            assert_eq!(offset, 6);
            Err::<((), usize), _>(VfsError::PermissionDenied)
        }),
        Err(VfsError::PermissionDenied)
    );
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(6));
    assert_eq!(
        file.with_current_position_transaction(4, |_offset| Ok(((), 5))),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(6));
}

#[test]
fn operation_position_transaction_hides_intermediate_cursor_from_concurrent_read() {
    let (file, _state) = append_test_file_with_access(
        NodeFlags::NON_CACHEABLE,
        16,
        FileFlags::READ | FileFlags::WRITE,
    );
    let file = Arc::new(file);
    let (holding_tx, holding_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    let transfer_file = file.clone();
    let transfer = thread::spawn(move || {
        transfer_file
            .with_current_position_transaction(8, |offset| {
                assert_eq!(offset, 0);
                holding_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                // Model multiple positioned chunks whose aggregate cursor
                // update must not become visible until this callback ends.
                Ok(((), 6))
            })
            .unwrap();
    });
    holding_rx.recv().unwrap();

    let (started_tx, started_rx) = mpsc::channel();
    let (observed_tx, observed_rx) = mpsc::channel();
    let observer_file = file.clone();
    let observer = thread::spawn(move || {
        started_tx.send(()).unwrap();
        let mut buf = [0u8; 2];
        let read = observer_file.read_slice(&mut buf).unwrap();
        let mut handle = observer_file.as_ref();
        let position = handle.seek(SeekFrom::Current(0)).unwrap();
        observed_tx.send((read, buf, position)).unwrap();
    });
    started_rx.recv().unwrap();
    assert_eq!(
        observed_rx.recv_timeout(Duration::from_millis(20)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );

    release_tx.send(()).unwrap();
    transfer.join().unwrap();
    let (read, buf, position) = observed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    observer.join().unwrap();
    assert_eq!(read, 2);
    assert_eq!(&buf, b"gh");
    assert_eq!(position, 8);
}

#[test]
fn same_description_transfer_can_write_at_its_frozen_position() {
    let (file, state) = append_test_file_with_access(
        NodeFlags::NON_CACHEABLE,
        8,
        FileFlags::READ | FileFlags::WRITE,
    );
    let mut buf = [0u8; 4];

    assert_eq!(
        file.read_slice_at_current_then(&mut buf, |data, offset| {
            assert_eq!(data, b"abcd");
            assert_eq!(offset, 0);
            file.write_at_slice(data, offset)
        }),
        Ok(2)
    );
    let mut handle = &file;
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(2));
    assert_eq!(&*state.read_offsets.lock(), &[0]);
    assert_eq!(&*state.write_offsets.lock(), &[0]);
}

#[test]
fn stream_read_and_append_status_write_use_zero_without_an_ofd_position() {
    let (file, state) = append_test_file_with_access(
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM,
        8,
        FileFlags::READ | FileFlags::WRITE | FileFlags::APPEND,
    );

    let mut bytes = [0u8; 2];
    let mut dst = Cursor::new(&mut bytes[..]);
    assert_eq!(file.read(&mut dst), Ok(2));
    assert_eq!(&bytes, b"ab");

    let mut src = Cursor::new(&b"xy"[..]);
    assert_eq!(file.write(&mut src), Ok(2));
    assert_eq!(&*state.read_offsets.lock(), &[0]);
    assert_eq!(&*state.write_offsets.lock(), &[0]);
    assert_eq!(state.append_calls.load(Ordering::Acquire), 0);
    assert!(!file.has_current_position());
    assert!(file.supports_positioned_read());
    assert!(file.supports_positioned_write());
    assert!(file.supports_seek());
    let mut handle = &file;
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(0));
}

#[test]
fn positioned_io_and_seek_capabilities_are_independent_of_stream_cursor() {
    let flags = NodeFlags::NON_CACHEABLE
        | NodeFlags::STREAM
        | NodeFlags::NO_POSITIONED_READ
        | NodeFlags::NO_POSITIONED_WRITE
        | NodeFlags::NO_SEEK;
    let (file, _state) = append_test_file_with_access(flags, 8, FileFlags::READ | FileFlags::WRITE);

    assert!(!file.has_current_position());
    assert!(!file.supports_positioned_read());
    assert!(!file.supports_positioned_write());
    assert!(!file.supports_seek());
}

#[test]
fn direct_multichunk_errors_return_and_publish_committed_prefixes() {
    let chunk = FileBackend::DIRECT_IO_CHUNK;

    let (reader, read_state) = append_test_file_with_access(
        NodeFlags::NON_CACHEABLE,
        (chunk * 2) as u64,
        FileFlags::READ,
    );
    read_state.full_page_io.store(true, Ordering::Release);
    read_state.fail_read_call.store(1, Ordering::Release);
    let mut output = vec![0u8; chunk * 2];
    let mut dst = Cursor::new(output.as_mut_slice());
    assert_eq!(reader.read(&mut dst), Ok(chunk));
    let mut reader_handle = &reader;
    assert_eq!(reader_handle.seek(SeekFrom::Current(0)), Ok(chunk as u64));
    assert_eq!(&*read_state.read_offsets.lock(), &[0, chunk as u64]);

    let (writer, write_state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 0, FileFlags::WRITE);
    write_state.full_page_io.store(true, Ordering::Release);
    write_state.fail_write_call.store(1, Ordering::Release);
    let input = vec![0x5a; chunk * 2];
    let mut src = Cursor::new(input.as_slice());
    assert_eq!(
        writer.write_with_placement(&mut src, WritePlacement::Current),
        Ok(chunk)
    );
    let mut writer_handle = &writer;
    assert_eq!(writer_handle.seek(SeekFrom::Current(0)), Ok(chunk as u64));
    assert_eq!(&*write_state.write_offsets.lock(), &[0, chunk as u64]);
}

#[test]
fn cached_aligned_bypass_multichunk_errors_preserve_completed_prefixes() {
    let chunk = ALIGNED_BYPASS_CHUNK;

    let (cached, _location, read_state) = cached_append_test_file((chunk * 2) as u64);
    read_state.full_page_io.store(true, Ordering::Release);
    read_state.fail_read_call.store(1, Ordering::Release);
    let reader = File::new(FileBackend::Cached(cached), FileFlags::READ);
    let mut output = vec![0u8; chunk * 2];
    let mut dst = Cursor::new(output.as_mut_slice());
    assert_eq!(reader.read(&mut dst), Ok(chunk));
    let mut reader_handle = &reader;
    assert_eq!(reader_handle.seek(SeekFrom::Current(0)), Ok(chunk as u64));
    assert_eq!(&*read_state.read_offsets.lock(), &[0, chunk as u64]);

    let (cached, _location, source_state) = cached_append_test_file(0);
    source_state.full_page_io.store(true, Ordering::Release);
    let source_writer = File::new(FileBackend::Cached(cached), FileFlags::WRITE);
    assert_eq!(
        source_writer
            .write_with_placement(FaultingReader::new(chunk * 2, 1), WritePlacement::Current,),
        Ok(chunk)
    );
    let mut source_writer_handle = &source_writer;
    assert_eq!(
        source_writer_handle.seek(SeekFrom::Current(0)),
        Ok(chunk as u64)
    );
    assert_eq!(&*source_state.write_offsets.lock(), &[0]);

    let (cached, _location, write_state) = cached_append_test_file(0);
    write_state.full_page_io.store(true, Ordering::Release);
    write_state.fail_write_call.store(1, Ordering::Release);
    let writer = File::new(FileBackend::Cached(cached), FileFlags::WRITE);
    let input = vec![0x5a; chunk * 2];
    let mut src = Cursor::new(input.as_slice());
    assert_eq!(
        writer.write_with_placement(&mut src, WritePlacement::Current),
        Ok(chunk)
    );
    let mut writer_handle = &writer;
    assert_eq!(writer_handle.seek(SeekFrom::Current(0)), Ok(chunk as u64));
    assert_eq!(&*write_state.write_offsets.lock(), &[0, chunk as u64]);
}

#[test]
fn cached_write_fault_before_first_byte_does_not_extend_inode() {
    let (cached, _location, state) = cached_append_test_file(0);
    let file = File::new(FileBackend::Cached(cached), FileFlags::WRITE);

    assert_eq!(
        file.write_with_placement(
            FaultingReader::new(PAGE_SIZE + 1, 0),
            WritePlacement::Current,
        ),
        Err(axio::Error::BadAddress)
    );
    assert_eq!(state.inode_len.load(Ordering::Acquire), 0);
    assert_eq!(state.set_len_calls.load(Ordering::Acquire), 0);
    let mut handle = &file;
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(0));
}

#[test]
fn cached_generic_second_page_fault_returns_and_publishes_first_page() {
    let (cached, _location, state) = cached_append_test_file(0);
    let file = File::new(FileBackend::Cached(cached), FileFlags::WRITE);

    assert_eq!(
        file.write_with_placement(
            FaultingReader::new(PAGE_SIZE + 1, 1),
            WritePlacement::Current,
        ),
        Ok(PAGE_SIZE)
    );
    assert_eq!(state.inode_len.load(Ordering::Acquire), PAGE_SIZE as u64);
    assert_eq!(state.set_len_calls.load(Ordering::Acquire), 1);
    let mut handle = &file;
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(PAGE_SIZE as u64));
}

#[test]
fn cached_read_second_page_error_returns_and_publishes_first_page() {
    let (cached, _location, state) = cached_append_test_file((PAGE_SIZE + 1) as u64);
    state.full_page_io.store(true, Ordering::Release);
    state.fail_read_call.store(1, Ordering::Release);
    let file = File::new(FileBackend::Cached(cached), FileFlags::READ);
    let mut output = vec![0u8; PAGE_SIZE + 1];

    assert_eq!(file.read_slice(&mut output), Ok(PAGE_SIZE));
    assert_eq!(&*state.read_offsets.lock(), &[0, PAGE_SIZE as u64]);
    let mut handle = &file;
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(PAGE_SIZE as u64));
}

#[test]
fn cached_pinned_second_page_errors_preserve_completed_prefixes() {
    let (reader, _location, read_state) = cached_append_test_file((PAGE_SIZE + 1) as u64);
    read_state.full_page_io.store(true, Ordering::Release);
    read_state.fail_read_call.store(1, Ordering::Release);
    let mut destination = vec![0u8; PAGE_SIZE + 1];
    let destination_segment = [PinnedPhysicalSegment::new(
        destination.as_mut_ptr() as usize,
        destination.len(),
    )];
    assert_eq!(
        unsafe { reader.read_at_pinned_segments(&destination_segment, 0, false) },
        Ok(PAGE_SIZE)
    );

    let (writer, _location, write_state) = cached_append_test_file(0);
    write_state.fail_set_len_call.store(1, Ordering::Release);
    let source = vec![0x6b; PAGE_SIZE + 1];
    let source_segment = [PinnedPhysicalSegment::new(
        source.as_ptr() as usize,
        source.len(),
    )];
    assert_eq!(
        unsafe { writer.write_at_pinned_segments(&source_segment, 0, false) },
        Ok(PAGE_SIZE)
    );
    assert_eq!(
        write_state.inode_len.load(Ordering::Acquire),
        PAGE_SIZE as u64
    );
    assert_eq!(write_state.set_len_calls.load(Ordering::Acquire), 2);
}

#[test]
fn direct_pinned_io_never_passes_user_physical_ranges_as_lower_slices() {
    let (_cached, location, state) = cached_append_test_file(8);
    let backend = FileBackend::Direct(location);

    let mut destination = vec![0u8; 8];
    let destination_start = destination.as_mut_ptr() as usize;
    let destination_end = destination_start + destination.len();
    let destination_segment = [PinnedPhysicalSegment::new(
        destination_start,
        destination.len(),
    )];
    assert_eq!(
        unsafe { backend.read_at_pinned_segments(&destination_segment, 0, true) },
        Ok(8)
    );
    let lower_read = state.last_read_buf.load(Ordering::Acquire);
    assert!(!(destination_start..destination_end).contains(&lower_read));

    let source = vec![0x4d; 8];
    let source_start = source.as_ptr() as usize;
    let source_end = source_start + source.len();
    let source_segment = [PinnedPhysicalSegment::new(source_start, source.len())];
    assert_eq!(
        unsafe { backend.write_at_pinned_segments(&source_segment, 0, true) },
        Ok(2)
    );
    let lower_write = state.last_write_buf.load(Ordering::Acquire);
    assert!(!(source_start..source_end).contains(&lower_write));
}

#[test]
fn default_vectored_loops_preserve_progress_before_later_errors() {
    let (reader, read_state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 2, FileFlags::READ);
    read_state.full_page_io.store(true, Ordering::Release);
    read_state.fail_read_call.store(1, Ordering::Release);
    let mut left = [0u8; 1];
    let mut right = [0u8; 1];
    let mut dst: [&mut [u8]; 2] = [&mut left, &mut right];
    assert_eq!(
        reader
            .location()
            .entry()
            .as_file()
            .unwrap()
            .read_at_vectored(&mut dst, 0),
        Ok(1)
    );

    let (writer, write_state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 0, FileFlags::WRITE);
    write_state.full_page_io.store(true, Ordering::Release);
    write_state.fail_write_call.store(1, Ordering::Release);
    let src: [&[u8]; 2] = [b"a", b"b"];
    assert_eq!(
        writer
            .location()
            .entry()
            .as_file()
            .unwrap()
            .write_at_vectored(&src, 0),
        Ok(1)
    );
}

#[test]
fn scalar_append_error_after_one_direct_chunk_publishes_the_prefix() {
    let chunk = FileBackend::DIRECT_IO_CHUNK;
    let (file, state) = append_test_file_with_access(NodeFlags::NON_CACHEABLE, 0, FileFlags::WRITE);
    state.fail_append_call.store(1, Ordering::Release);
    let input = vec![0x41; chunk * 2];
    let mut src = Cursor::new(input.as_slice());

    assert_eq!(
        file.write_with_placement(&mut src, WritePlacement::End),
        Ok(chunk)
    );
    assert_eq!(state.append_calls.load(Ordering::Acquire), 2);
    assert_eq!(state.inode_len.load(Ordering::Acquire), chunk as u64);
    let mut handle = &file;
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(chunk as u64));
}

#[test]
fn different_ofds_cannot_admit_append_against_the_same_stale_eof() {
    let state = AppendTestState::new(0);
    let fs = Filesystem::new(RegistryTestFs::new_for_append(
        NodeFlags::NON_CACHEABLE,
        state.clone(),
    ));
    let location = Mountpoint::new_root(&fs).root_location();
    let left = File::new(FileBackend::Direct(location.clone()), FileFlags::WRITE);
    let right = File::new(FileBackend::Direct(location), FileFlags::WRITE);
    let start = Arc::new(Barrier::new(3));

    let spawn = |file: File, marker: u8, start: Arc<Barrier>| {
        thread::spawn(move || {
            start.wait();
            let bytes = [marker];
            let mut src = Cursor::new(&bytes[..]);
            file.write_with_placement_and_admission(
                &mut src,
                WritePlacement::End,
                |offset, requested| {
                    if offset >= 1 {
                        Err(VfsError::OutOfRange)
                    } else {
                        Ok(requested)
                    }
                },
            )
        })
    };
    let left = spawn(left, b'L', start.clone());
    let right = spawn(right, b'R', start.clone());
    start.wait();
    let results = [left.join().unwrap(), right.join().unwrap()];

    assert_eq!(results.iter().filter(|result| **result == Ok(1)).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Err(VfsError::OutOfRange))
            .count(),
        1
    );
    assert_eq!(state.append_calls.load(Ordering::Acquire), 1);
    assert_eq!(state.inode_len.load(Ordering::Acquire), 1);
}

#[test]
fn same_ofd_append_admission_keeps_operation_order_and_cursor() {
    let (file, state) = append_test_file_with_access(NodeFlags::NON_CACHEABLE, 0, FileFlags::WRITE);
    let file = Arc::new(file);
    let offsets = Arc::new(StdMutex::new(Vec::new()));
    let start = Arc::new(Barrier::new(3));

    let spawn =
        |file: Arc<File>, marker: u8, start: Arc<Barrier>, offsets: Arc<StdMutex<Vec<u64>>>| {
            thread::spawn(move || {
                start.wait();
                let bytes = [marker];
                let mut src = Cursor::new(&bytes[..]);
                file.write_with_placement_and_admission(
                    &mut src,
                    WritePlacement::End,
                    |offset, requested| {
                        offsets.lock().unwrap().push(offset);
                        Ok(requested)
                    },
                )
            })
        };
    let left = spawn(file.clone(), b'A', start.clone(), offsets.clone());
    let right = spawn(file.clone(), b'B', start.clone(), offsets.clone());
    start.wait();
    assert_eq!(left.join().unwrap(), Ok(1));
    assert_eq!(right.join().unwrap(), Ok(1));

    assert_eq!(&*offsets.lock().unwrap(), &[0, 1]);
    assert_eq!(state.inode_len.load(Ordering::Acquire), 2);
    let mut handle = file.as_ref();
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(2));
}

#[test]
fn competing_unaligned_append_cannot_move_eof_after_admission() {
    let state = AppendTestState::new(512);
    let fs = Filesystem::new(RegistryTestFs::new_for_append(
        NodeFlags::NON_CACHEABLE,
        state.clone(),
    ));
    let location = Mountpoint::new_root(&fs).root_location();
    let aligned = File::new(FileBackend::Direct(location.clone()), FileFlags::WRITE);
    let unaligned = File::new(FileBackend::Direct(location), FileFlags::WRITE);
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    let first = thread::spawn(move || {
        let bytes = vec![b'A'; 512];
        let mut src = Cursor::new(bytes.as_slice());
        aligned.write_at_end_with_admission(&mut src, |offset, requested| {
            assert_eq!(offset, 512);
            admitted_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(requested)
        })
    });
    admitted_rx.recv().unwrap();

    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let second = thread::spawn(move || {
        started_tx.send(()).unwrap();
        let result = unaligned.write_at_end_slice(b"u");
        done_tx.send(result).unwrap();
    });
    started_rx.recv().unwrap();
    assert_eq!(
        done_rx.recv_timeout(Duration::from_millis(20)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    assert_eq!(state.append_calls.load(Ordering::Acquire), 0);

    release_tx.send(()).unwrap();
    assert_eq!(first.join().unwrap(), Ok(512));
    assert_eq!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap(), Ok(1));
    second.join().unwrap();
    assert_eq!(&*state.append_markers.lock().unwrap(), b"Au");
    assert_eq!(state.inode_len.load(Ordering::Acquire), 1025);
}

#[test]
fn explicit_end_ignores_non_append_default_for_all_write_forms() {
    let (file, state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
    let mut src = Cursor::new(&b"ab"[..]);
    assert_eq!(
        file.write_with_placement_and_admission(
            &mut src,
            WritePlacement::End,
            |offset, requested| {
                assert_eq!(offset, 41);
                Ok(requested)
            },
        ),
        Ok(2)
    );
    assert_eq!(state.append_calls.load(Ordering::Acquire), 1);
    assert_eq!(state.inode_len.load(Ordering::Acquire), 43);
    let mut handle = &file;
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(43));

    let (file, state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
    assert_eq!(
        file.write_slice_with_placement(b"abc", WritePlacement::End),
        Ok(3)
    );
    assert_eq!(state.append_calls.load(Ordering::Acquire), 1);
    assert_eq!(state.inode_len.load(Ordering::Acquire), 44);

    let (file, state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
    assert_eq!(
        file.write_vectored_slice_with_placement(&[b"ab", b"c"], WritePlacement::End),
        Ok(3)
    );
    assert_eq!(state.append_calls.load(Ordering::Acquire), 2);
    assert_eq!(state.inode_len.load(Ordering::Acquire), 44);
    assert_eq!(
        file.write_slice_with_placement(b"x", WritePlacement::Current),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(&*state.write_offsets.lock(), &[44]);
}

#[test]
fn vectored_append_preserves_partial_and_error_position_semantics() {
    let (file, state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
    state.append_limit.store(1, Ordering::Release);
    assert_eq!(
        file.write_vectored_slice_with_placement(&[b"ab", b"cd"], WritePlacement::End),
        Ok(1)
    );
    assert_eq!(state.append_calls.load(Ordering::Acquire), 1);
    assert_eq!(state.inode_len.load(Ordering::Acquire), 42);
    assert_eq!(
        file.write_slice_with_placement(b"x", WritePlacement::Current),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(&*state.write_offsets.lock(), &[42]);

    let (file, state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
    state.fail_append_call.store(1, Ordering::Release);
    assert_eq!(
        file.write_vectored_slice_with_placement(&[b"ab", b"cd"], WritePlacement::End),
        Ok(2)
    );
    assert_eq!(state.append_calls.load(Ordering::Acquire), 2);
    assert_eq!(state.inode_len.load(Ordering::Acquire), 43);
    assert_eq!(
        file.write_slice_with_placement(b"x", WritePlacement::Current),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(&*state.write_offsets.lock(), &[43]);
    let mut handle = &file;
    assert_eq!(handle.seek(SeekFrom::Current(0)), Ok(43));
}

#[test]
fn concurrent_vectored_appends_keep_each_scatter_list_contiguous() {
    const WRITES: usize = 512;

    let state = AppendTestState::new(0);
    state.yield_after_append.store(true, Ordering::Release);
    let fs = Filesystem::new(RegistryTestFs::new_for_append(
        NodeFlags::NON_CACHEABLE,
        state.clone(),
    ));
    let location = Mountpoint::new_root(&fs).root_location();
    let left = Arc::new(File::new(
        FileBackend::Direct(location.clone()),
        FileFlags::WRITE,
    ));
    let right = Arc::new(File::new(FileBackend::Direct(location), FileFlags::WRITE));
    let start = Arc::new(Barrier::new(3));

    let left_thread = {
        let file = left.clone();
        let start = start.clone();
        thread::spawn(move || {
            start.wait();
            for _ in 0..WRITES {
                file.write_at_end_vectored_slice(&[b"A", b"a"]).unwrap();
            }
        })
    };
    let right_thread = {
        let file = right.clone();
        let start = start.clone();
        thread::spawn(move || {
            start.wait();
            for _ in 0..WRITES {
                file.write_at_end_vectored_slice(&[b"B", b"b"]).unwrap();
            }
        })
    };
    start.wait();
    left_thread.join().unwrap();
    right_thread.join().unwrap();

    let markers = state.append_markers.lock().unwrap();
    assert_eq!(markers.len(), WRITES * 4);
    assert!(
        markers
            .chunks_exact(2)
            .all(|pair| pair == b"Aa" || pair == b"Bb")
    );
    assert_eq!(state.inode_len.load(Ordering::Acquire), (WRITES * 4) as u64);
}

#[test]
fn positioned_end_writes_do_not_change_ofd_position() {
    let (file, state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
    let mut src = Cursor::new(&b"ab"[..]);
    assert_eq!(
        file.write_at_end_with_admission(&mut src, |offset, requested| {
            assert_eq!(offset, 41);
            Ok(requested)
        }),
        Ok(2)
    );
    assert_eq!(
        file.write_slice_with_placement(b"x", WritePlacement::Current),
        Ok(1)
    );
    assert_eq!(&*state.write_offsets.lock(), &[0]);

    let (file, state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
    assert_eq!(file.write_at_end_slice(b"ab"), Ok(2));
    assert_eq!(
        file.write_slice_with_placement(b"x", WritePlacement::Current),
        Ok(1)
    );
    assert_eq!(&*state.write_offsets.lock(), &[0]);

    let (file, state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
    assert_eq!(file.write_at_end_vectored_slice(&[b"a", b"b"]), Ok(2));
    assert_eq!(
        file.write_slice_with_placement(b"x", WritePlacement::Current),
        Ok(1)
    );
    assert_eq!(&*state.write_offsets.lock(), &[0]);
}

#[test]
fn positioned_append_write_forms_use_and_advance_ofd_offset() {
    let flags = NodeFlags::NON_CACHEABLE | NodeFlags::POSITIONED_APPEND;

    let (file, state) = append_test_file(flags, 97);
    let mut src = Cursor::new(&b"abcd"[..]);
    assert_eq!(file.write(&mut src), Ok(2));
    let mut second = Cursor::new(&b"z"[..]);
    assert_eq!(file.write(&mut second), Err(VfsError::InvalidInput));
    assert_positioned_append_offsets(&state);

    let (file, state) = append_test_file(flags, 97);
    assert_eq!(file.write_slice(b"abcd"), Ok(2));
    assert_eq!(file.write_slice(b"z"), Err(VfsError::InvalidInput));
    assert_positioned_append_offsets(&state);

    let (file, state) = append_test_file(flags, 97);
    assert_eq!(file.write_vectored_slice(&[b"abcd", b"ef"]), Ok(2));
    assert_eq!(
        file.write_vectored_slice(&[b"z"]),
        Err(VfsError::InvalidInput)
    );
    assert_positioned_append_offsets(&state);
}

#[test]
fn ordinary_append_still_uses_inode_append() {
    let (file, state) = append_test_file(NodeFlags::NON_CACHEABLE, 41);

    assert_eq!(file.write_slice(b"abc"), Ok(3));
    assert_eq!(state.append_calls.load(Ordering::Acquire), 1);
    assert!(state.write_offsets.lock().is_empty());
    assert_eq!(state.inode_len.load(Ordering::Acquire), 44);
}

#[test]
fn append_status_can_change_without_changing_write_authority() {
    let (file, state) = append_test_file(NodeFlags::NON_CACHEABLE, 41);
    file.set_append(false);
    assert_eq!(file.write_slice(b"abc"), Ok(2));
    assert_eq!(state.append_calls.load(Ordering::Acquire), 0);
    assert_eq!(&*state.write_offsets.lock(), &[0]);

    let (file, state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
    file.set_append(true);
    assert!(file.flags().contains(FileFlags::APPEND));
    assert_eq!(file.write_slice(b"abc"), Ok(3));
    assert_eq!(state.append_calls.load(Ordering::Acquire), 1);

    let (file, state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::WRITE);
    file.set_append(true);
    file.set_append(false);
    assert!(!file.flags().contains(FileFlags::APPEND));
    assert_eq!(file.write_slice(b"abc"), Ok(2));
    assert_eq!(state.append_calls.load(Ordering::Acquire), 0);
    assert_eq!(&*state.write_offsets.lock(), &[0]);

    let (file, _state) =
        append_test_file_with_access(NodeFlags::NON_CACHEABLE, 41, FileFlags::READ);
    file.set_append(true);
    assert!(file.flags().contains(FileFlags::APPEND));
    assert!(matches!(
        file.access(FileFlags::APPEND),
        Err(VfsError::BadFileDescriptor)
    ));
    assert_eq!(file.write_slice(b"x"), Err(VfsError::BadFileDescriptor));
}

struct RegistryTestFs {
    this: Weak<Self>,
    append: Option<(NodeFlags, Arc<AppendTestState>)>,
}

impl RegistryTestFs {
    fn new() -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            append: None,
        })
    }

    fn new_for_append(flags: NodeFlags, state: Arc<AppendTestState>) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            append: Some((flags, state)),
        })
    }
}

impl FilesystemOps for RegistryTestFs {
    fn name(&self) -> &str {
        "registry-test"
    }

    fn root_dir(&self) -> DirEntry {
        let fs = self.this.upgrade().expect("test filesystem is live");
        let node: Arc<dyn FileNodeOps> = if let Some((flags, state)) = &self.append {
            Arc::new(AppendTestFile {
                flags: *flags,
                state: state.clone(),
                fs,
            })
        } else {
            Arc::new(RegistryTestFile { fs })
        };
        DirEntry::new_file(
            FileNode::new(node),
            NodeType::RegularFile,
            Reference::root(),
        )
    }

    fn stat(&self) -> VfsResult<StatFs> {
        Ok(StatFs {
            fs_type: 0,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            file_count: 1,
            free_file_count: 0,
            name_length: 255,
            fragment_size: 4096,
            mount_flags: 0,
        })
    }
}

struct RegistryTestFile {
    fs: Arc<RegistryTestFs>,
}

impl NodeOps for RegistryTestFile {
    fn inode(&self) -> u64 {
        1
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        Ok(Metadata {
            device: 0,
            inode: 1,
            nlink: 0,
            mode: NodePermission::from_bits_truncate(0o600),
            node_type: NodeType::RegularFile,
            uid: 0,
            gid: 0,
            project_id: 0,
            size: 0,
            block_size: 4096,
            blocks: 0,
            rdev: Default::default(),
            atime: axfs_ng_vfs::Timestamp::ZERO,
            btime: axfs_ng_vfs::Timestamp::ZERO,
            mtime: axfs_ng_vfs::Timestamp::ZERO,
            ctime: axfs_ng_vfs::Timestamp::ZERO,
        })
    }

    fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
        Ok(())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        &*self.fs
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

impl Pollable for RegistryTestFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}

impl FileNodeOps for RegistryTestFile {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        Ok((buf.len(), buf.len() as u64))
    }

    fn set_len(&self, _len: u64) -> VfsResult<()> {
        Ok(())
    }

    fn set_symlink(&self, _target: &FsPath) -> VfsResult<()> {
        Ok(())
    }
}

#[test]
fn unlinked_last_close_releases_registry_retention_and_anchor() {
    let fs = Filesystem::new(RegistryTestFs::new());
    let identity = fs.identity_weak();
    let mountpoint = Mountpoint::new_root(&fs);
    let location = mountpoint.root_location();
    let key = cached_file_registry_key(&location);
    let shared = Arc::new(CachedFileShared::new(key, true));
    shared.unlinked.store(true, Ordering::Release);
    shared.open_handles.store(1, Ordering::Release);

    let retained_pages_before = CLOSED_FILE_CACHE_RETAINED_PAGES.load(Ordering::Acquire);
    let mut entry = FileUserData::new(&location, &shared);
    assert!(entry.retain_closed(&location, &shared, 3).is_none());
    entry.writeback_anchor = Some(location.writeback_anchor());
    let retired = file_cache_registry().lock().insert(key, entry);
    assert!(retired.is_none());
    assert_eq!(Arc::strong_count(&shared), 2);
    assert_eq!(
        CLOSED_FILE_CACHE_RETAINED_PAGES.load(Ordering::Acquire),
        retained_pages_before + 3
    );

    let cached = CachedFile {
        inner: location,
        shared: shared.clone(),
        in_memory: true,
    };
    drop(mountpoint);
    drop(fs);
    drop(cached);

    assert!(!file_cache_registry().lock().contains_key(&key));
    assert_eq!(Arc::strong_count(&shared), 1);
    assert_eq!(
        CLOSED_FILE_CACHE_RETAINED_PAGES.load(Ordering::Acquire),
        retained_pages_before
    );
    assert!(identity.upgrade().is_none());
}

#[test]
fn unlinked_cleanup_waits_for_live_direct_range_lease() {
    let fs = Filesystem::new(RegistryTestFs::new());
    let mountpoint = Mountpoint::new_root(&fs);
    let location = mountpoint.root_location();
    let key = cached_file_registry_key(&location);
    let shared = Arc::new(CachedFileShared::new(key, true));
    let retired = file_cache_registry()
        .lock()
        .insert(key, FileUserData::new(&location, &shared));
    assert!(retired.is_none());

    let lease = CachedFileShared::try_range_cache_lease(
        &shared,
        0..PAGE_SIZE as u64,
        RangeCacheLeaseKind::DirectWrite,
    )
    .unwrap();
    mark_cached_file_unlinked(&location);
    assert!(shared.unlinked_cleanup_pending.load(Ordering::Acquire));
    assert!(file_cache_registry().lock().contains_key(&key));

    // The unlink path must not panic or discard around a live direct
    // effect. The exact lease drop is the synchronous cleanup trigger.
    drop(lease);
    assert!(!shared.unlinked_cleanup_pending.load(Ordering::Acquire));
    assert!(!file_cache_registry().lock().contains_key(&key));

    drop(mountpoint);
    drop(fs);
}

#[test]
fn close_of_last_direct_range_lease_completes_unlinked_cleanup() {
    let fs = Filesystem::new(RegistryTestFs::new());
    let mountpoint = Mountpoint::new_root(&fs);
    let location = mountpoint.root_location();
    let key = cached_file_registry_key(&location);
    let shared = Arc::new(CachedFileShared::new(key, true));
    assert!(
        file_cache_registry()
            .lock()
            .insert(key, FileUserData::new(&location, &shared))
            .is_none()
    );

    let lease = CachedFileShared::try_range_cache_lease(
        &shared,
        0..PAGE_SIZE as u64,
        RangeCacheLeaseKind::DirectRead,
    )
    .unwrap();
    mark_cached_file_unlinked(&location);
    assert!(shared.unlinked_cleanup_pending.load(Ordering::Acquire));

    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let closer = thread::spawn({
        let ready = ready.clone();
        let release = release.clone();
        move || {
            ready.wait();
            release.wait();
            drop(lease);
        }
    });
    ready.wait();
    assert!(file_cache_registry().lock().contains_key(&key));
    release.wait();
    closer.join().unwrap();

    assert!(!shared.unlinked_cleanup_pending.load(Ordering::Acquire));
    assert!(!file_cache_registry().lock().contains_key(&key));
    drop(mountpoint);
    drop(fs);
}

#[test]
fn unlinked_registry_release_does_not_remove_replacement_shared() {
    let fs = Filesystem::new(RegistryTestFs::new());
    let mountpoint = Mountpoint::new_root(&fs);
    let location = mountpoint.root_location();
    let key = cached_file_registry_key(&location);
    let replacement = Arc::new(CachedFileShared::new(key, true));
    let stale = Arc::new(CachedFileShared::new(key, true));
    let mut entry = FileUserData::new(&location, &replacement);
    entry.writeback_anchor = Some(location.writeback_anchor());
    let retired = file_cache_registry().lock().insert(key, entry);
    assert!(retired.is_none());

    release_unlinked_cached_file_registry_ownership(&location, &stale);

    {
        let registry = file_cache_registry().lock();
        let entry = registry.get(&key).expect("replacement entry must remain");
        assert!(entry.references_shared(&replacement));
        assert!(entry.writeback_anchor.is_some());
    }
    let retired = file_cache_registry().lock().remove(&key);
    drop(retired);
}

#[test]
fn released_shared_does_not_remove_replacement_registry_entry() {
    let fs = Filesystem::new(RegistryTestFs::new());
    let mountpoint = Mountpoint::new_root(&fs);
    let location = mountpoint.root_location();
    let key = cached_file_registry_key(&location);
    let stale = Arc::new(CachedFileShared::new(key, true));
    let replacement = Arc::new(CachedFileShared::new(key, true));
    let retired = file_cache_registry()
        .lock()
        .insert(key, FileUserData::new(&location, &replacement));
    assert!(retired.is_none());

    drop(stale);

    {
        let registry = file_cache_registry().lock();
        let entry = registry.get(&key).expect("replacement entry must remain");
        assert!(entry.references_shared(&replacement));
    }
    let retired = file_cache_registry().lock().remove(&key);
    drop(retired);
}

#[test]
fn ordinary_linked_cached_file_close_removes_dead_registry_entry() {
    let fs = Filesystem::new(RegistryTestFs::new());
    let mountpoint = Mountpoint::new_root(&fs);
    let location = mountpoint.root_location();
    let key = cached_file_registry_key(&location);
    let cached = CachedFile::get_or_create(location);
    assert!(file_cache_registry().lock().contains_key(&key));

    drop(cached);

    assert!(!file_cache_registry().lock().contains_key(&key));
}

#[test]
fn inode_generation_replacement_gets_distinct_cache_identity() {
    let fs = Filesystem::new(RegistryTestFs::new());
    let mountpoint = Mountpoint::new_root(&fs);
    let location = mountpoint.root_location();

    let first = CachedFile::get_or_create(location.clone());
    let first_identity = first.identity();

    // Model the backend publishing a new inode-generation attachment in
    // the same device/inode slot after unlink.  The old cache remains
    // live, so a raw pair key would incorrectly merge the two states.
    let replacement = FileUserData::new_identity(&location);
    assert_eq!(replacement.registry_key.device(), first_identity.device());
    assert_eq!(replacement.registry_key.inode(), first_identity.inode());
    assert_ne!(replacement.registry_key.object(), first_identity.object());
    location.user_data().insert(replacement);

    let second = CachedFile::get_or_create(location);
    assert_ne!(first.identity(), second.identity());
    assert!(!first.ptr_eq(&second));
    {
        let registry = file_cache_registry().lock();
        assert!(registry.contains_key(&first_identity));
        assert!(registry.contains_key(&second.identity()));
    }
    remove_cached_file_registry_entry(first_identity.device(), first_identity.inode());
    assert!(
        file_cache_registry()
            .lock()
            .contains_key(&second.identity())
    );

    drop(first);
    drop(second);
    assert!(!file_cache_registry().lock().contains_key(&first_identity));
}

#[test]
fn direct_only_shared_release_removes_dead_registry_entry() {
    let fs = Filesystem::new(RegistryTestFs::new());
    let mountpoint = Mountpoint::new_root(&fs);
    let location = mountpoint.root_location();
    let key = cached_file_registry_key(&location);
    let shared = cached_file_shared_for_location_or_create(&location);
    assert!(file_cache_registry().lock().contains_key(&key));

    drop(shared);

    assert!(!file_cache_registry().lock().contains_key(&key));
}

#[test]
fn final_shared_release_preserves_writeback_anchor() {
    let fs = Filesystem::new(RegistryTestFs::new());
    let mountpoint = Mountpoint::new_root(&fs);
    let location = mountpoint.root_location();
    let key = cached_file_registry_key(&location);
    let shared = Arc::new(CachedFileShared::new(key, true));
    let mut entry = FileUserData::new(&location, &shared);
    entry.writeback_anchor = Some(location.writeback_anchor());
    let retired = file_cache_registry().lock().insert(key, entry);
    assert!(retired.is_none());

    drop(shared);

    {
        let registry = file_cache_registry().lock();
        let entry = registry.get(&key).expect("writeback anchor must remain");
        assert!(!entry.has_live_shared());
        assert!(entry.writeback_anchor.is_some());
    }
    let retired = file_cache_registry().lock().remove(&key);
    drop(retired);
}
