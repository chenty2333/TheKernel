//! Shared cached-file state, range leases and page-invalidation transactions.

use super::*;

/// A page evicted while inserting a new cached file page.
#[must_use = "eviction metadata must be observed by the cache caller"]
pub struct EvictedPage {
    pub(super) pn: u32,
}

impl EvictedPage {
    /// Returns the file page number that was evicted.
    pub fn page_number(&self) -> u32 {
        self.pn
    }
}

pub(super) fn per_file_page_cache_capacity() -> NonZeroUsize {
    pub(super) const MIB: usize = 1024 * 1024;
    pub(super) const GIB: usize = 1024 * MIB;
    let ram = total_ram_size();
    let pages = if ram <= 512 * MIB {
        64
    } else if ram <= 2 * GIB {
        2048
    } else {
        let extra_gib = (ram - 2 * GIB) / GIB;
        2048usize.saturating_add(extra_gib.saturating_mul(256))
    };
    NonZeroUsize::new(pages).unwrap()
}

pub(super) struct EvictListener {
    pub(super) owner: CachedFileEvictionOwner,
    pub(super) listener: EvictPrepareFn,
    pub(super) link: LinkedListAtomicLink,
}

intrusive_adapter!(pub(super) EvictListenerAdapter = Box<EvictListener>: EvictListener { link: LinkedListAtomicLink });

pub(super) const RANGE_CACHE_LEASE_SLOTS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RangeCacheLeaseKind {
    DirectRead,
    DirectWrite,
    CachedRead,
    CachedWrite,
    WholeFileMutation,
}

#[inline]
pub(super) fn range_lease_drop_requests_unlinked_cleanup(kind: RangeCacheLeaseKind) -> bool {
    matches!(
        kind,
        RangeCacheLeaseKind::DirectRead | RangeCacheLeaseKind::DirectWrite
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RangeCacheLeaseRecord {
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) generation: u64,
    pub(super) kind: RangeCacheLeaseKind,
}

pub(super) struct RangeCacheLeaseTable {
    pub(super) slots: [Option<RangeCacheLeaseRecord>; RANGE_CACHE_LEASE_SLOTS],
    pub(super) generations: [u64; RANGE_CACHE_LEASE_SLOTS],
}

impl RangeCacheLeaseTable {
    pub(super) const fn new() -> Self {
        Self {
            slots: [None; RANGE_CACHE_LEASE_SLOTS],
            generations: [0; RANGE_CACHE_LEASE_SLOTS],
        }
    }

    pub(super) fn conflicts(
        candidate: RangeCacheLeaseRecord,
        active: RangeCacheLeaseRecord,
    ) -> bool {
        // Extent-changing operations exclude every in-flight direct request,
        // even when their cached-byte effects cover only a tail or one page.
        if matches!(
            (candidate.kind, active.kind),
            (
                RangeCacheLeaseKind::WholeFileMutation,
                RangeCacheLeaseKind::DirectRead | RangeCacheLeaseKind::DirectWrite,
            ) | (
                RangeCacheLeaseKind::DirectRead | RangeCacheLeaseKind::DirectWrite,
                RangeCacheLeaseKind::WholeFileMutation,
            )
        ) {
            return true;
        }
        if candidate.end <= active.start || active.end <= candidate.start {
            return false;
        }
        match (candidate.kind, active.kind) {
            (RangeCacheLeaseKind::CachedRead, RangeCacheLeaseKind::CachedRead)
            | (RangeCacheLeaseKind::CachedRead, RangeCacheLeaseKind::CachedWrite)
            | (RangeCacheLeaseKind::CachedWrite, RangeCacheLeaseKind::CachedRead)
            | (RangeCacheLeaseKind::CachedWrite, RangeCacheLeaseKind::CachedWrite) => false,
            _ => true,
        }
    }
}

/// A fixed-capacity, generation-checked lease for one file-cache byte range.
/// The token owns the slot; callers may release all cache locks before a
/// synchronous device wait and retain only this lease while the request is
/// live.
pub(super) struct RangeCacheLease {
    pub(super) shared: Arc<CachedFileShared>,
    pub(super) slot: usize,
    pub(super) generation: u64,
    pub(super) record: RangeCacheLeaseRecord,
}

impl RangeCacheLease {
    pub(super) fn revalidate(&self) -> bool {
        self.shared
            .range_cache_leases
            .lock()
            .slots
            .get(self.slot)
            .and_then(|slot| *slot)
            .is_some_and(|active| active == self.record && active.generation == self.generation)
    }
}

impl Drop for RangeCacheLease {
    fn drop(&mut self) {
        let request_cleanup = range_lease_drop_requests_unlinked_cleanup(self.record.kind);
        let mut table = self.shared.range_cache_leases.lock();
        if table
            .slots
            .get(self.slot)
            .and_then(|slot| *slot)
            .is_some_and(|active| active == self.record && active.generation == self.generation)
        {
            table.slots[self.slot] = None;
        }
        drop(table);
        // An effect/direct-I/O lease can be the only thing keeping an
        // unlinked file from acquiring its whole-file mutation lease. Do the
        // cleanup from this exact last-lease transition instead of panicking
        // in the unlink path or relying on open-handle accounting.
        if request_cleanup {
            request_unlinked_cached_file_cleanup(&self.shared);
        }
    }
}

pub(super) struct CachedFileShared {
    /// Registry slot owned weakly by this shared state. Final release removes
    /// it only when both this key and this allocation still match.
    pub(super) registry_key: CachedFileRegistryKey,
    /// Keeps the non-owning registry key unique for this inode generation
    /// while a retained cache or futex lease is still alive.
    pub(super) identity_lease: Arc<CachedFileIdentityLease>,
    /// tmpfs and ALWAYS_CACHE files have no lower storage from which clean
    /// pages can be faulted back, so global pressure reclaim must skip them.
    pub(super) in_memory: bool,
    pub(super) page_cache: Mutex<LruCache<u32, PageCache>>,
    /// Remaining entries in the current bounded pressure-scan cycle. The LRU
    /// rotation is the cursor; this counter prevents an all-ineligible inode
    /// from requesting active retries forever.
    pub(super) pressure_reclaim_scan_remaining: AtomicUsize,
    pub(super) pressure_reclaim_scan_epoch: AtomicU64,
    /// Completion edge for address-space aliases temporarily fenced while a
    /// page is being evicted.  The counter is changed before waking so users
    /// can use the normal observe-arm-observe wait pattern without losing a
    /// commit or abort that races listener registration.
    pub(super) eviction_completion_epoch: AtomicU64,
    pub(super) eviction_completion: WaitQueue,
    pub(super) evict_listeners: Mutex<LinkedList<EvictListenerAdapter>>,
    pub(super) unlinked: AtomicBool,
    /// Set once an unlinked inode has no open handles but a direct/effect
    /// range lease still prevents whole-file cache discard. The last
    /// relevant lease drop synchronously retries the bounded cleanup.
    pub(super) unlinked_cleanup_pending: AtomicBool,
    /// Serializes cleanup attempts so a lease drop racing an earlier Busy
    /// observation cannot lose the deferred request.
    pub(super) unlinked_cleanup_lock: Mutex<()>,
    pub(super) open_handles: AtomicUsize,
    pub(super) mount_roots: AtomicUsize,
    pub(super) user_io_pin_admission: Mutex<CachedFilePinAdmission>,
    /// Sleeping inode transaction gate.  Unlike the old spin `RwLock`, this
    /// may remain held while backing I/O waits; range leases and mutation
    /// admission provide the fine-grained exclusion semantics.
    pub(super) direct_io_lock: SleepingMutex<()>,
    /// Serializes dirty writeback with truncate/cache length transitions.
    pub(super) writeback_lock: RwLock<()>,
    /// Serializes O_APPEND transaction boundaries across handles for this inode.
    pub(super) append_lock: RwLock<()>,
    /// Fixed-capacity range ownership used to arbitrate cache aliases and
    /// direct I/O without holding a lock across device completion.
    pub(super) range_cache_leases: Mutex<RangeCacheLeaseTable>,
    /// Per-inode async range-writeback admission and completion state.  This
    /// is deliberately shared by every CachedFile opened on the inode.
    pub(super) range_writeback: RangeWritebackState,
    pub(super) fadvise_readahead: FadviseReadaheadState,
    /// The last lower-visible EOF observed by a cache-owned operation.  It is
    /// only an admission snapshot: a NOWAIT write must never refresh it by
    /// querying the provider, and therefore returns EAGAIN until it is known.
    pub(super) known_file_len: AtomicU64,
    pub(super) known_file_len_valid: AtomicBool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FadviseReadaheadRequest {
    pub(super) offset: u64,
    pub(super) len: u64,
}
pub(super) struct FadviseReadaheadQueue {
    pub(super) worker_running: bool,
    pub(super) worker_generation: u64,
    pub(super) head: usize,
    pub(super) len: usize,
    pub(super) pending: [Option<FadviseReadaheadRequest>; FADVISE_READAHEAD_QUEUE_CAPACITY],
}

impl FadviseReadaheadQueue {
    pub(super) const fn new() -> Self {
        Self {
            worker_running: false,
            worker_generation: 0,
            head: 0,
            len: 0,
            pending: [None; FADVISE_READAHEAD_QUEUE_CAPACITY],
        }
    }

    pub(super) fn contains(&self, request: FadviseReadaheadRequest) -> bool {
        (0..self.len).any(|index| {
            self.pending[(self.head + index) % FADVISE_READAHEAD_QUEUE_CAPACITY]
                .is_some_and(|queued| queued == request)
        })
    }

    pub(super) fn push(&mut self, request: FadviseReadaheadRequest) -> bool {
        if self.len == FADVISE_READAHEAD_QUEUE_CAPACITY {
            return false;
        }
        let tail = (self.head + self.len) % FADVISE_READAHEAD_QUEUE_CAPACITY;
        self.pending[tail] = Some(request);
        self.len += 1;
        true
    }

    pub(super) fn pop(&mut self) -> Option<FadviseReadaheadRequest> {
        if self.len == 0 {
            return None;
        }
        let request = self.pending[self.head].take();
        self.head = (self.head + 1) % FADVISE_READAHEAD_QUEUE_CAPACITY;
        self.len -= 1;
        request
    }
}
pub(super) struct FadviseReadaheadState {
    pub(super) queue: Mutex<FadviseReadaheadQueue>,
}

pub(super) struct RangeWritebackRequest {
    pub(super) generation: u64,
    pub(super) offset: u64,
    pub(super) len: u64,
    pub(super) data_only: bool,
}

pub(super) struct RangeWritebackCompletion {
    pub(super) generation: u64,
    pub(super) offset: u64,
    pub(super) len: u64,
    pub(super) result: VfsResult<()>,
}

#[derive(Default)]
pub(super) struct RangeWritebackQueue {
    pub(super) next_generation: u64,
    pub(super) worker_running: bool,
    pub(super) active: Option<(u64, u64, u64)>,
    pub(super) pending: VecDeque<RangeWritebackRequest>,
    pub(super) completed: Vec<RangeWritebackCompletion>,
    pub(super) interests: BTreeMap<u64, usize>,
}

pub(super) struct RangeWritebackState {
    pub(super) queue: Mutex<RangeWritebackQueue>,
    pub(super) completed: WaitQueue,
}

pub(super) fn gc_range_writeback_completions(queue: &mut RangeWritebackQueue) {
    if let Some((&last, _)) = queue.interests.last_key_value() {
        queue
            .completed
            .retain(|completion| completion.generation <= last);
    } else {
        queue.completed.clear();
    }
}

/// A generation interest acquired atomically with a range snapshot or write
/// submission.  Its Drop is the sole completion-retention release point.
pub struct RangeWritebackFence {
    pub(super) shared: Option<Arc<CachedFileShared>>,
    pub(super) generation: u64,
}

impl RangeWritebackFence {
    pub(super) fn none() -> Self {
        Self {
            shared: None,
            generation: 0,
        }
    }
}

impl Drop for RangeWritebackFence {
    fn drop(&mut self) {
        let Some(shared) = self.shared.take() else {
            return;
        };
        let mut queue = shared.range_writeback.queue.lock();
        if let Some(count) = queue.interests.get_mut(&self.generation) {
            *count -= 1;
            if *count == 0 {
                queue.interests.remove(&self.generation);
            }
        }
        gc_range_writeback_completions(&mut queue);
    }
}

impl RangeWritebackState {
    pub(super) fn new() -> Self {
        Self {
            queue: Mutex::new(RangeWritebackQueue::default()),
            completed: WaitQueue::new(),
        }
    }
}

impl CachedFileShared {
    pub(super) fn cachestat(&self, first_page: u64, last_page: u64) -> CachedFileCacheStat {
        if first_page > last_page {
            return CachedFileCacheStat::default();
        }
        let recent_threshold =
            u64::try_from(FILE_CACHE_ACTIVE_PAGES.load(Ordering::Acquire)).unwrap_or(u64::MAX);
        let cache = self.page_cache.lock();
        let mut stat = CachedFileCacheStat::default();
        for (page_no, page) in cache.iter() {
            if (first_page..=last_page).contains(&u64::from(*page_no)) {
                stat.nr_cache += 1;
                stat.nr_dirty += u64::from(page.is_dirty());
                stat.nr_writeback += u64::from(page.is_writeback());
            }
        }
        let shadows = file_cache_shadows().lock();
        // Sample age only after joining the shadow publication domain, so a
        // reclaimer cannot publish A+1 between the entry observation and the
        // age snapshot used to classify A.
        let age = current_file_cache_nonresident_age();
        for (key, evicted_at) in shadows.iter() {
            if key.identity == self.registry_key
                && (first_page..=last_page).contains(&u64::from(key.page))
                && !cache.contains(&key.page)
            {
                stat.nr_evicted += 1;
                stat.nr_recently_evicted += u64::from(file_cache_shadow_is_recent(
                    age,
                    *evicted_at,
                    recent_threshold,
                ));
            }
        }
        stat
    }

    #[cfg(test)]
    pub(super) fn new(registry_key: CachedFileRegistryKey, in_memory: bool) -> Self {
        Self::with_identity(
            registry_key,
            Arc::new(CachedFileIdentityLease {
                object: registry_key.object(),
                discarded_unlinked: AtomicBool::new(false),
            }),
            in_memory,
        )
    }

    pub(super) fn with_identity(
        registry_key: CachedFileRegistryKey,
        identity_lease: Arc<CachedFileIdentityLease>,
        in_memory: bool,
    ) -> Self {
        // tmpfs/ALWAYS_CACHE pages are the file's authoritative contents,
        // not a replaceable disk cache. Grow their index with resident pages;
        // imposing an LRU eviction limit would manufacture OOM at that file
        // size even while physical memory remains available. `unbounded()`
        // starts empty, and PageCache allocation still reports real pressure.
        let page_cache = if in_memory {
            LruCache::unbounded()
        } else {
            LruCache::new(per_file_page_cache_capacity())
        };
        Self {
            registry_key,
            identity_lease,
            in_memory,
            page_cache: Mutex::new(page_cache),
            pressure_reclaim_scan_remaining: AtomicUsize::new(0),
            pressure_reclaim_scan_epoch: AtomicU64::new(0),
            eviction_completion_epoch: AtomicU64::new(0),
            eviction_completion: WaitQueue::new(),
            evict_listeners: Mutex::new(LinkedList::default()),
            unlinked: AtomicBool::new(false),
            unlinked_cleanup_pending: AtomicBool::new(false),
            unlinked_cleanup_lock: Mutex::new(()),
            open_handles: AtomicUsize::new(0),
            mount_roots: AtomicUsize::new(0),
            user_io_pin_admission: Mutex::new(CachedFilePinAdmission::default()),
            direct_io_lock: SleepingMutex::new(()),
            writeback_lock: RwLock::new(()),
            append_lock: RwLock::new(()),
            range_cache_leases: Mutex::new(RangeCacheLeaseTable::new()),
            range_writeback: RangeWritebackState::new(),
            fadvise_readahead: FadviseReadaheadState {
                queue: Mutex::new(FadviseReadaheadQueue::new()),
            },
            known_file_len: AtomicU64::new(0),
            known_file_len_valid: AtomicBool::new(false),
        }
    }

    pub(super) fn observe_file_len(&self, len: u64) {
        self.known_file_len.store(len, Ordering::Release);
        self.known_file_len_valid.store(true, Ordering::Release);
    }

    pub(super) fn nowait_write_within_known_len(&self, end: u64) -> bool {
        self.known_file_len_valid.load(Ordering::Acquire)
            && end <= self.known_file_len.load(Ordering::Acquire)
    }

    pub(super) fn try_range_cache_lease(
        shared: &Arc<Self>,
        range: Range<u64>,
        kind: RangeCacheLeaseKind,
    ) -> VfsResult<RangeCacheLease> {
        if range.start >= range.end {
            return Err(VfsError::InvalidInput);
        }
        let record = RangeCacheLeaseRecord {
            start: range.start,
            end: range.end,
            generation: 0,
            kind,
        };
        let mut table = shared.range_cache_leases.lock();
        if table
            .slots
            .iter()
            .flatten()
            .any(|active| RangeCacheLeaseTable::conflicts(record, *active))
        {
            return Err(VfsError::ResourceBusy);
        }
        let Some(slot) = table.slots.iter().position(Option::is_none) else {
            return Err(VfsError::ResourceBusy);
        };
        let generation = table.generations[slot]
            .checked_add(1)
            .ok_or(VfsError::NoMemory)?;
        table.generations[slot] = generation;
        let record = RangeCacheLeaseRecord {
            generation,
            ..record
        };
        table.slots[slot] = Some(record);
        drop(table);
        Ok(RangeCacheLease {
            shared: shared.clone(),
            slot,
            generation,
            record,
        })
    }
}

#[derive(Clone)]
pub(super) enum InvalidationShadowDomain {
    All,
    Range(Range<u32>),
    From(u64),
}

impl InvalidationShadowDomain {
    pub(super) fn contains(&self, page: u32) -> bool {
        match self {
            Self::All => true,
            Self::Range(range) => range.contains(&page),
            Self::From(first) => u64::from(page) >= *first,
        }
    }
}

pub(super) struct CachedPageInvalidationTransaction {
    pub(super) shared: Arc<CachedFileShared>,
    pub(super) pages: Vec<(u32, PageCache)>,
    /// Stable, owner-deduplicated listener set captured after mutation
    /// admission and before any cache page is detached.
    pub(super) listeners: Vec<EvictListenerSnapshot>,
    /// Temporary alias fences.  Dropping this field aborts all prepared
    /// aliases before the staged cache pages are restored.
    pub(super) reservations: Option<CachedPageEvictionReservations>,
    /// Pre-removal MRU-to-LRU key order for a pageout rollback.  LruCache
    /// exposes promotion but not insertion-at-position, so restoring this
    /// order after reinsertion preserves the page's original LRU placement.
    pub(super) rollback_lru_order: Option<Vec<u32>>,
    /// Shadow keys to clear for invalidate.  This is collected before page
    /// detach so commit never allocates after alias teardown.
    pub(super) shadow_cleanup_keys: Vec<CachedFileShadowKey>,
    /// Exact no-fail credit for the pageout shadow.  It is acquired before
    /// detach, consumed by commit, or released by Drop on every rollback.
    pub(super) shadow_publication: Option<FileCacheShadowPublicationReservation>,
    pub(super) kind: CachedPageTransactionKind,
    pub(super) shadow_domain: Option<InvalidationShadowDomain>,
    pub(super) committed: bool,
}

/// The cache slot left behind by an operation is part of its semantics:
/// invalidation makes a prior reclaim shadow inapplicable, whereas successful
/// pageout is a reclaim event and publishes a new workingset shadow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CachedPageTransactionKind {
    Invalidate,
    Pageout,
}

impl CachedPageInvalidationTransaction {
    pub(super) fn new(mutation: &CachedFileMutationGuard) -> Self {
        Self::new_shared(mutation.shared.clone())
    }

    pub(super) fn new_shared(shared: Arc<CachedFileShared>) -> Self {
        Self {
            shared,
            pages: Vec::new(),
            listeners: Vec::new(),
            reservations: None,
            rollback_lru_order: None,
            shadow_cleanup_keys: Vec::new(),
            shadow_publication: None,
            kind: CachedPageTransactionKind::Invalidate,
            shadow_domain: None,
            committed: false,
        }
    }

    pub(super) fn new_pageout(mutation: &CachedFileMutationGuard) -> Self {
        Self {
            shared: mutation.shared.clone(),
            pages: Vec::new(),
            listeners: Vec::new(),
            reservations: None,
            rollback_lru_order: None,
            shadow_cleanup_keys: Vec::new(),
            shadow_publication: None,
            kind: CachedPageTransactionKind::Pageout,
            shadow_domain: None,
            committed: false,
        }
    }

    pub(super) fn stage_all(&mut self) -> VfsResult<()> {
        self.shadow_domain = Some(InvalidationShadowDomain::All);
        self.prepare_shadow_cleanup_keys()?;
        self.listeners = evict_listeners_snapshot(&self.shared)?;
        let mut cache = self.shared.page_cache.lock();
        if cache.iter().any(|(_, page)| page.is_pinned()) {
            return Err(VfsError::ResourceBusy);
        }
        self.pages
            .try_reserve_exact(cache.len())
            .map_err(|_| VfsError::NoMemory)?;
        let mut lru_order = Vec::new();
        lru_order
            .try_reserve_exact(cache.len())
            .map_err(|_| VfsError::NoMemory)?;
        lru_order.extend(cache.iter().map(|(pn, _)| *pn));
        while let Some((pn, page)) = cache.pop_lru() {
            file_cache_remove_page(&page);
            self.pages.push((pn, page));
        }
        self.rollback_lru_order = Some(lru_order);
        Ok(())
    }

    pub(super) fn stage_range(&mut self, pages: Range<u32>) -> VfsResult<usize> {
        self.shadow_domain = Some(InvalidationShadowDomain::Range(pages.clone()));
        self.prepare_shadow_cleanup_keys()?;
        self.listeners = evict_listeners_snapshot(&self.shared)?;
        let mut cache = self.shared.page_cache.lock();
        let mut keys = Vec::new();
        keys.try_reserve_exact(cache.len())
            .map_err(|_| VfsError::NoMemory)?;
        // The advised byte range may cover an enormous sparse file. Walk the
        // bounded resident cache once rather than taking the cache lock for
        // every theoretical page in that range.
        for (pn, _) in cache.iter() {
            if *pn >= pages.start && *pn < pages.end {
                keys.push(*pn);
            }
        }
        if keys
            .iter()
            .any(|pn| cache.peek(pn).is_some_and(PageCache::is_pinned))
        {
            return Err(VfsError::ResourceBusy);
        }
        self.pages
            .try_reserve_exact(keys.len())
            .map_err(|_| VfsError::NoMemory)?;
        let mut lru_order = Vec::new();
        lru_order
            .try_reserve_exact(cache.len())
            .map_err(|_| VfsError::NoMemory)?;
        lru_order.extend(cache.iter().map(|(pn, _)| *pn));
        for pn in keys {
            if let Some(page) = cache.pop(&pn) {
                file_cache_remove_page(&page);
                self.pages.push((pn, page));
            }
        }
        self.rollback_lru_order = Some(lru_order);
        let count = self.pages.len();
        Ok(count)
    }

    /// Detaches one evictable page without notifying its mappings yet.
    ///
    /// Pageout writes dirty data before its mappings are detached.  Keeping
    /// the page in this transaction until the listener acknowledgement has
    /// succeeded makes a failed acknowledgement lossless: `Drop` restores the
    /// original (still dirty) page to the cache.
    pub(super) fn stage_page_for_pageout(&mut self, pn: u32) -> VfsResult<bool> {
        debug_assert_eq!(self.kind, CachedPageTransactionKind::Pageout);
        self.listeners = evict_listeners_snapshot(&self.shared)?;
        let cache = self.shared.page_cache.lock();
        let Some(page) = cache.peek(&pn) else {
            return Ok(false);
        };
        if page.is_pinned() || page.is_writeback() {
            return Ok(false);
        }
        // Reserve all rollback ownership before unlinking the page.  Once the
        // page is detached, every fallible operation must be able to restore
        // it without allocating or losing its original state.
        self.pages
            .try_reserve_exact(1)
            .map_err(|_| VfsError::NoMemory)?;
        let mut lru_order = Vec::new();
        lru_order
            .try_reserve_exact(cache.len())
            .map_err(|_| VfsError::NoMemory)?;
        lru_order.extend(cache.iter().map(|(cached_pn, _)| *cached_pn));
        // The listener and all rollback allocations are now prepared, while
        // the resident page is still linked.  Drop the cache lock before
        // entering the global shadow domain: cachestat takes that order too.
        drop(cache);
        let shadow_publication = prepare_file_cache_shadow_publication(&self.shared)?;
        let mut cache = self.shared.page_cache.lock();
        // The transaction's mutation guard excludes conflicting cache
        // invalidation; still revalidate this page after releasing the lock
        // for the shadow reservation so a benign miss leaves no stale token.
        let Some(page) = cache.peek(&pn) else {
            return Ok(false);
        };
        if page.is_pinned() || page.is_writeback() {
            return Ok(false);
        }
        let page = cache
            .pop(&pn)
            .expect("page cache entry disappeared while holding its lock");
        file_cache_remove_page(&page);
        self.pages.push((pn, page));
        self.rollback_lru_order = Some(lru_order);
        self.shadow_publication = Some(shadow_publication);
        Ok(true)
    }

    pub(super) fn stage_from(&mut self, first_page: u64) -> VfsResult<usize> {
        self.shadow_domain = Some(InvalidationShadowDomain::From(first_page));
        self.prepare_shadow_cleanup_keys()?;
        self.listeners = evict_listeners_snapshot(&self.shared)?;
        let mut cache = self.shared.page_cache.lock();
        let mut keys = Vec::new();
        keys.try_reserve_exact(cache.len())
            .map_err(|_| VfsError::NoMemory)?;
        for (pn, _) in cache.iter() {
            if u64::from(*pn) >= first_page {
                keys.push(*pn);
            }
        }
        if keys
            .iter()
            .any(|pn| cache.peek(pn).is_some_and(PageCache::is_pinned))
        {
            return Err(VfsError::ResourceBusy);
        }
        self.pages
            .try_reserve_exact(keys.len())
            .map_err(|_| VfsError::NoMemory)?;
        let mut lru_order = Vec::new();
        lru_order
            .try_reserve_exact(cache.len())
            .map_err(|_| VfsError::NoMemory)?;
        lru_order.extend(cache.iter().map(|(pn, _)| *pn));
        for pn in keys {
            if let Some(page) = cache.pop(&pn) {
                file_cache_remove_page(&page);
                self.pages.push((pn, page));
            }
        }
        self.rollback_lru_order = Some(lru_order);
        let count = self.pages.len();
        Ok(count)
    }

    /// Fence every existing alias before writeback.  All allocation happens
    /// before the first prepare callback, and no cache/listener lock is held
    /// while callbacks acquire an address space.
    pub(super) fn prepare_evictions(&mut self) -> VfsResult<()> {
        let capacity = self
            .pages
            .len()
            .checked_mul(self.listeners.len())
            .ok_or(VfsError::NoMemory)?;
        let mut reservations = CachedPageEvictionReservations::reserve(capacity)?;
        for (pn, page) in &self.pages {
            reservations.prepare(
                &self.listeners,
                CachedPageEviction {
                    identity: self.shared.registry_key,
                    page_number: *pn,
                    paddr: page.paddr(),
                    writeback_only: false,
                },
            )?;
        }
        self.reservations = Some(reservations);
        Ok(())
    }

    pub(super) fn prepare_shadow_cleanup_keys(&mut self) -> VfsResult<()> {
        let Some(domain) = self.shadow_domain.as_ref() else {
            return Ok(());
        };
        let shadows = file_cache_shadows().lock();
        let count = shadows
            .iter()
            .filter(|(key, _)| {
                key.identity == self.shared.registry_key && domain.contains(key.page)
            })
            .count();
        self.shadow_cleanup_keys
            .try_reserve_exact(count)
            .map_err(|_| VfsError::NoMemory)?;
        for (key, _) in shadows.iter() {
            if key.identity == self.shared.registry_key && domain.contains(key.page) {
                self.shadow_cleanup_keys.push(*key);
            }
        }
        Ok(())
    }

    // Cache-invalidation machinery for the in-progress write path.
    #[allow(dead_code)]
    pub(super) fn writeback(&mut self, file: &FileNode, range_flush: bool) -> VfsResult<()> {
        let native_mutation = begin_file_node_writeback_mutation(file)?;
        self.writeback_with_held_native_gate(
            file,
            range_flush,
            held_native_writeback_gate(&native_mutation),
        )
    }

    pub(super) fn writeback_with_held_native_gate(
        &mut self,
        file: &FileNode,
        range_flush: bool,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<()> {
        for (pn, page) in &mut self.pages {
            let written =
                writeback_cached_page_data_with_held_native_gate(file, *pn, page, native_gate)?;
            if written != 0 {
                record_dirty_writeback(range_flush, 1, written, false);
            }
        }
        Ok(())
    }

    pub(super) fn restore_staged_page(
        &mut self,
        pn: u32,
        update: impl FnOnce(&mut PageCache),
    ) -> bool {
        let Some(index) = self
            .pages
            .iter()
            .position(|(staged_pn, _)| *staged_pn == pn)
        else {
            return false;
        };
        let (staged_pn, mut page) = self.pages.swap_remove(index);
        update(&mut page);
        let mut cache = self.shared.page_cache.lock();
        file_cache_restore_page(&page);
        assert!(
            cache.put(staged_pn, page).is_none(),
            "restoring a retained truncate page replaced page {staged_pn}"
        );
        cache.demote(&staged_pn); // LRU recency only; preserves PageCache::active.
        true
    }

    pub(super) fn commit_discard(mut self) {
        // `commit` is deliberately infallible; every alias was fully prepared
        // (including page-table resources) before backing I/O was allowed.
        if let Some(reservations) = self.reservations.take() {
            reservations.commit();
        }
        match self.kind {
            CachedPageTransactionKind::Invalidate => {
                clear_file_cache_shadow_keys(&self.shadow_cleanup_keys);
            }
            CachedPageTransactionKind::Pageout => {
                // A shadow is published only after writeback and every
                // eviction listener have acknowledged the detached page.
                // Drop before this point restores the resident page and
                // deliberately leaves no shadow behind.
                debug_assert_eq!(self.pages.len(), 1);
                let (pn, _) = self
                    .pages
                    .first()
                    .expect("pageout commit without a staged page");
                record_prepared_file_cache_shadow(
                    self.shadow_publication
                        .take()
                        .expect("pageout commit without shadow reservation"),
                    &self.shared,
                    *pn,
                );
            }
        }
        for (_, page) in &mut self.pages {
            page.clear_dirty();
        }
        self.committed = true;
    }
}

impl Drop for CachedPageInvalidationTransaction {
    fn drop(&mut self) {
        if self.committed || self.pages.is_empty() {
            return;
        }
        // Reopen writable aliases before publishing the rolled-back resident
        // page.  Besides matching the transaction's stated ordering, this
        // prevents a concurrent writer from observing a restored cache page
        // behind a still-fenced PTE and turning a writeback failure into a
        // spurious protection fault.
        drop(self.reservations.take());
        let mut cache = self.shared.page_cache.lock();
        for (pn, page) in self.pages.drain(..).rev() {
            file_cache_restore_page(&page);
            assert!(
                cache.put(pn, page).is_none(),
                "cache invalidation rollback replaced page {pn}"
            );
            cache.demote(&pn); // LRU recency only; preserves PageCache::active.
        }
        if let Some(order) = self.rollback_lru_order.take() {
            // Iterating LRU-to-MRU and promoting each key reconstructs the
            // captured MRU-to-LRU order without allocating while rolling back.
            for pn in order.iter().rev() {
                cache.promote(pn);
            }
        }
    }
}
