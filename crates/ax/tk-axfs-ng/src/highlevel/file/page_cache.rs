//! The per-file page cache, page pins and eviction reservations.

use super::*;

/// A single page-sized cache entry backed by a physical page.
#[derive(Debug)]
pub struct PageCache {
    pub(super) addr: VirtAddr,
    pub(super) dirty: bool,
    pub(super) prefetched: bool,
    pub(super) noreuse: bool,
    pub(super) referenced: bool,
    pub(super) active: bool,
    pub(super) pins: u32,
    pub(super) writeback: u32,
    pub(super) shmem: bool,
}

/// Per-file page-cache counters returned to the native cachestat adapter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachedFileCacheStat {
    pub nr_cache: u64,
    pub nr_dirty: u64,
    pub nr_writeback: u64,
    pub nr_evicted: u64,
    pub nr_recently_evicted: u64,
}

pub(super) static IN_MEMORY_PAGE_CACHE_RESIDENT_PAGES: AtomicUsize = AtomicUsize::new(0);

pub fn in_memory_page_cache_pages() -> usize {
    IN_MEMORY_PAGE_CACHE_RESIDENT_PAGES.load(Ordering::Acquire)
}

impl PageCache {
    pub(super) fn new(shmem: bool) -> VfsResult<Self> {
        let addr = global_allocator()
            .alloc_pages(1, PAGE_SIZE, UsageKind::PageCache)
            .inspect_err(|err| {
                ratelimit::warn_ratelimited!("Failed to allocate page cache: {:?}", err);
            })?;
        if shmem {
            IN_MEMORY_PAGE_CACHE_RESIDENT_PAGES.fetch_add(1, Ordering::Release);
        }
        Ok(Self {
            addr: addr.into(),
            dirty: false,
            prefetched: false,
            noreuse: false,
            referenced: false,
            active: false,
            pins: 0,
            writeback: 0,
            shmem,
        })
    }

    /// Returns the physical address of this page.
    pub fn paddr(&self) -> PhysAddr {
        virtual_to_physical(self.addr)
    }

    /// Marks this page as dirty so it will be flushed on eviction.
    pub fn mark_dirty(&mut self) {
        self.prefetched = false;
        self.noreuse = false;
        self.referenced = true;
        self.dirty = true;
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub(super) fn is_pinned(&self) -> bool {
        self.pins != 0
    }

    pub(super) fn is_writeback(&self) -> bool {
        self.writeback != 0
    }

    pub(super) fn mark_prefetched(&mut self) {
        self.prefetched = true;
    }

    pub(super) fn clear_prefetched(&mut self) -> bool {
        let was_prefetched = self.prefetched;
        self.prefetched = false;
        was_prefetched
    }

    pub(super) fn mark_noreuse(&mut self) {
        self.noreuse = true;
    }

    pub(super) fn is_noreuse(&self) -> bool {
        self.noreuse
    }

    // Reclaim-policy accessor for the in-progress readahead path.
    #[allow(dead_code)]
    pub(super) fn is_prefetched(&self) -> bool {
        self.prefetched
    }

    pub(super) fn record_reference(&mut self) -> bool {
        if self.referenced {
            let promoted = !self.active;
            self.active = true;
            if promoted {
                self.referenced = false;
            }
            promoted
        } else {
            self.referenced = true;
            false
        }
    }

    pub(super) fn is_active(&self) -> bool {
        self.active
    }

    pub(super) fn activate_refault(&mut self) {
        self.active = true;
        self.referenced = false;
    }

    pub(super) fn demote_active(&mut self) {
        self.active = false;
    }

    // Reclaim-policy accessor for the in-progress readahead path.
    #[allow(dead_code)]
    pub(super) fn is_referenced(&self) -> bool {
        self.referenced
    }

    pub(super) fn is_unused_prefetched(&self) -> bool {
        (self.prefetched || self.noreuse)
            && !self.dirty
            && !self.is_pinned()
            && !self.is_writeback()
    }

    pub(super) fn pin(&mut self) -> VfsResult<()> {
        self.pins = self.pins.checked_add(1).ok_or(VfsError::NoMemory)?;
        Ok(())
    }

    pub(super) fn unpin(&mut self) {
        assert!(self.pins > 0, "unpinning unpinned page cache entry");
        self.pins -= 1;
    }

    pub(super) fn begin_writeback(&mut self) -> VfsResult<()> {
        self.pin()?;
        match self.writeback.checked_add(1) {
            Some(writeback) => {
                self.writeback = writeback;
                Ok(())
            }
            None => {
                self.unpin();
                Err(VfsError::NoMemory)
            }
        }
    }

    pub(super) fn end_writeback(&mut self) {
        assert!(self.writeback > 0, "ending inactive page cache writeback");
        self.writeback -= 1;
        self.unpin();
    }

    pub(super) fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Returns a mutable slice over the page data.
    pub fn data(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.addr.as_mut_ptr(), PAGE_SIZE) }
    }
}

impl Drop for PageCache {
    fn drop(&mut self) {
        if self.is_writeback() {
            warn!("page cache entry dropped with writeback in flight");
        }
        if self.is_pinned() {
            warn!("pinned page cache entry dropped");
        }
        if self.dirty {
            warn!("dirty page dropped without flushing");
        }
        global_allocator().dealloc_pages(self.addr.as_usize(), 1, UsageKind::PageCache);
        if self.shmem {
            let _ = IN_MEMORY_PAGE_CACHE_RESIDENT_PAGES.try_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |n| n.checked_sub(1),
            );
        }
    }
}

/// A short-lived guard that prevents a cached file page from being evicted.
pub struct CachedFilePagePin {
    pub(super) cache: CachedFile,
    pub(super) pn: u32,
    pub(super) dirty_on_release: bool,
    pub(super) _range_lease: Option<RangeCacheLease>,
}

impl Drop for CachedFilePagePin {
    fn drop(&mut self) {
        let mut guard = self.cache.shared.page_cache.lock();
        let Some(page) = guard.get_mut(&self.pn) else {
            ratelimit::warn_ratelimited!(
                "CachedFilePagePin::drop: missing pinned cached page {}",
                self.pn
            );
            return;
        };
        if self.dirty_on_release {
            page.mark_dirty();
        }
        page.unpin();
        drop(guard);
        if self.dirty_on_release {
            retain_cached_file_writeback_anchor_if_dirty(&self.cache.inner, &self.cache.shared);
        }
    }
}

/// A conservative preparation window for file-backed user I/O pins.
pub struct CachedFilePinWindow {
    pub(super) cache: CachedFile,
    pub(super) _range_lease: Option<RangeCacheLease>,
}

impl Drop for CachedFilePinWindow {
    fn drop(&mut self) {
        let mut admission = self.cache.shared.user_io_pin_admission.lock();
        assert!(
            admission.pin_windows != 0,
            "cached-file pin-window underflow"
        );
        admission.pin_windows -= 1;
    }
}

#[derive(Default)]
pub(super) struct CachedFilePinAdmission {
    pub(super) cache_users: usize,
    pub(super) pin_windows: usize,
    pub(super) invalidating: bool,
}

/// Shared admission for ordinary page-cache users that may need an LRU slot.
pub(super) struct CachedFileCacheUserGuard {
    pub(super) shared: Arc<CachedFileShared>,
    pub(super) _range_lease: Option<RangeCacheLease>,
}

impl Drop for CachedFileCacheUserGuard {
    fn drop(&mut self) {
        let mut admission = self.shared.user_io_pin_admission.lock();
        assert!(
            admission.cache_users != 0,
            "cached-file user-count underflow"
        );
        admission.cache_users -= 1;
    }
}

/// Serializes a cache-invalidating file mutation with user-I/O pin admission.
///
/// Existing precise page pins are checked separately before the inode mutation
/// is committed. While this guard is alive, new preparation windows and page
/// pins fail without observing a half-published cache transition.
pub(super) struct CachedFileMutationGuard {
    pub(super) shared: Arc<CachedFileShared>,
    pub(super) _range_lease: Option<RangeCacheLease>,
}

impl Drop for CachedFileMutationGuard {
    fn drop(&mut self) {
        let mut admission = self.shared.user_io_pin_admission.lock();
        debug_assert!(
            admission.invalidating,
            "ending inactive cached-file mutation"
        );
        admission.invalidating = false;
    }
}

/// Stable identity of one address space that consumes cache-eviction notices.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CachedFileEvictionOwner(NonZeroUsize);

impl CachedFileEvictionOwner {
    /// Creates an owner key from a stable nonzero address-space identity.
    pub const fn new(key: usize) -> Option<Self> {
        match NonZeroUsize::new(key) {
            Some(key) => Some(Self(key)),
            None => None,
        }
    }

    /// Returns the underlying identity value.
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

/// Immutable identity of a page whose aliases are being fenced.
///
/// Listeners deliberately do not receive `PageCache`: the retained cache page
/// remains owned by the coordinator until every alias reservation commits.
#[derive(Clone, Copy, Debug)]
pub struct CachedPageEviction {
    /// Cache identity shared by every file mapping of this inode.
    pub identity: CachedFileIdentity,
    /// File-relative page number.
    pub page_number: u32,
    /// Physical frame still owned by the cache transaction.
    pub paddr: PhysAddr,
    /// Keep the frame resident and its aliases read-only after writeback.
    /// A later mapped write must fault to record the next dirty generation.
    pub writeback_only: bool,
}

/// One prepared address-space participant in a cache eviction.
///
/// Both operations are required to be allocation-free and infallible.  A
/// reservation must restore the original permissions when aborted. Commit
/// removes aliases for eviction, or leaves them read-only for writeback.
pub trait CachedPageEvictionReservation: Send {
    /// Publishes alias removal or the completed read-only writeback state.
    fn commit(self: Box<Self>);

    /// Reverses the temporary eviction fence/write protection.
    fn abort(self: Box<Self>);
}

pub(super) type EvictPrepareFn = Arc<
    dyn Fn(CachedPageEviction) -> VfsResult<Box<dyn CachedPageEvictionReservation>> + Send + Sync,
>;

#[derive(Clone)]
pub(super) struct EvictListenerSnapshot {
    pub(super) owner: CachedFileEvictionOwner,
    pub(super) listener: EvictPrepareFn,
}

pub(super) fn evict_listeners_snapshot(
    shared: &CachedFileShared,
) -> VfsResult<Vec<EvictListenerSnapshot>> {
    let listeners = shared.evict_listeners.lock();
    let mut snapshot = Vec::new();
    snapshot
        .try_reserve_exact(listeners.iter().count())
        .map_err(|_| VfsError::NoMemory)?;
    for listener in listeners.iter() {
        if snapshot
            .iter()
            .any(|existing: &EvictListenerSnapshot| existing.owner == listener.owner)
        {
            continue;
        }
        snapshot.push(EvictListenerSnapshot {
            owner: listener.owner,
            listener: listener.listener.clone(),
        });
    }
    Ok(snapshot)
}

/// RAII set of prepared alias participants.  Failed prepare and writeback
/// paths automatically abort in reverse order; cache rollback is then handled
/// by `CachedPageInvalidationTransaction::Drop`.
pub(super) struct CachedPageEvictionReservations {
    pub(super) reservations: Vec<Box<dyn CachedPageEvictionReservation>>,
    pub(super) committed: bool,
}

impl CachedPageEvictionReservations {
    pub(super) fn reserve(count: usize) -> VfsResult<Self> {
        let mut reservations = Vec::new();
        reservations
            .try_reserve_exact(count)
            .map_err(|_| VfsError::NoMemory)?;
        Ok(Self {
            reservations,
            committed: false,
        })
    }

    pub(super) fn prepare(
        &mut self,
        listeners: &[EvictListenerSnapshot],
        eviction: CachedPageEviction,
    ) -> VfsResult<()> {
        for listener in listeners {
            // The listener snapshot is deduplicated by owner before staging,
            // so one reservation scans all aliases in a single address space.
            self.reservations.push((listener.listener)(eviction)?);
        }
        Ok(())
    }

    pub(super) fn commit(mut self) {
        for reservation in self.reservations.drain(..) {
            reservation.commit();
        }
        self.committed = true;
    }
}

impl Drop for CachedPageEvictionReservations {
    fn drop(&mut self) {
        if !self.committed {
            while let Some(reservation) = self.reservations.pop() {
                reservation.abort();
            }
        }
    }
}
