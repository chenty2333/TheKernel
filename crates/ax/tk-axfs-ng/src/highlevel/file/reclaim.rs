//! Writeback anchors, clean-page reclaim and closed-file cache retention.

use super::*;

/// Reserve the already-existing registry writeback anchor for an immediate
/// NOWAIT cache write.  This deliberately never creates a registry entry or
/// waits for either bookkeeping lock: the caller must return EAGAIN before it
/// mutates a page when the durable dirty-page owner cannot be installed.
pub(super) struct NowaitWritebackAnchorGuard {
    pub(super) key: CachedFileRegistryKey,
    pub(super) shared: Arc<CachedFileShared>,
    pub(super) installed: bool,
    pub(super) committed: bool,
}

impl NowaitWritebackAnchorGuard {
    pub(super) fn commit(mut self) {
        self.committed = true;
    }
}

/// A NOWAIT request may complete only when every terminal action is already
/// nonblocking. Sync and cache-eviction still lack retained try-only permits;
/// timestamp publication has its own inode overlay reservation.
pub(super) fn nowait_terminal_plan_supported(
    opcode: FileIoOpcode,
    policy: FileIoPolicy,
    update_atime: bool,
) -> bool {
    if policy.sync != FileIoSyncMode::None || policy.dontcache {
        return false;
    }
    #[cfg(feature = "times")]
    {
        let _ = (opcode, update_atime);
        // Timestamp publication is backed by an inode-generation overlay
        // reservation acquired before submission/usercopy.
        true
    }
    #[cfg(not(feature = "times"))]
    {
        let _ = (opcode, update_atime);
        true
    }
}

/// Native immutable/append admission for the NOWAIT route.  A provider which
/// advertises file attributes but cannot obtain its own current lock-free
/// snapshot is not treated as unrestricted: it returns `EAGAIN`.  By
/// contrast, a node with no provider preserves the existing VFS behavior.
pub(super) fn try_begin_native_location_mutation_nowait(
    location: &Location,
    append: bool,
) -> VfsResult<Option<FileAttrMutationGuard>> {
    pub(super) const FS_XFLAG_IMMUTABLE: u64 = 0x0000_0008;
    pub(super) const FS_XFLAG_APPEND: u64 = 0x0000_0010;
    if location.file_attr_provider().is_none() {
        return Ok(None);
    }
    let data = location
        .entry()
        .persistent_user_data()
        .ok_or(VfsError::WouldBlock)?;
    let guard = data.try_begin_file_attr_mutation()?;
    // Re-read after entering the stable admission domain.  A setter cannot
    // publish native flags until this guard is released.
    let attr = location.try_get_file_attr()?;
    if attr.xflags & FS_XFLAG_IMMUTABLE != 0 {
        return Err(VfsError::OperationNotPermitted);
    }
    if attr.xflags & FS_XFLAG_APPEND != 0 && !append {
        return Err(VfsError::OperationNotPermitted);
    }
    Ok(Some(guard))
}

pub(super) fn begin_native_location_mutation(
    location: &Location,
    append: bool,
) -> VfsResult<Option<FileAttrMutationGuard>> {
    if location.file_attr_provider().is_none() {
        return Ok(None);
    }
    let data = location
        .entry()
        .persistent_user_data()
        .ok_or(VfsError::OperationNotSupported)?;
    let guard = data.begin_file_attr_mutation();
    let attr = location.get_file_attr()?;
    pub(super) const FS_XFLAG_IMMUTABLE: u64 = 0x0000_0008;
    pub(super) const FS_XFLAG_APPEND: u64 = 0x0000_0010;
    if attr.xflags & FS_XFLAG_IMMUTABLE != 0 {
        return Err(VfsError::OperationNotPermitted);
    }
    if attr.xflags & FS_XFLAG_APPEND != 0 && !append {
        return Err(VfsError::OperationNotPermitted);
    }
    Ok(Some(guard))
}

/// Holds the source inode's fileattr admission while flushing dirty data for
/// an extent-sharing operation.  Unlike a destination mutation this does not
/// reinterpret an already-admitted dirty page as a new IMMUTABLE/APPEND
/// write: it only prevents a setter from changing the source policy halfway
/// through the required writeback.
pub(super) fn begin_source_location_writeback_mutation(
    location: &Location,
) -> VfsResult<Option<FileAttrMutationGuard>> {
    if location.file_attr_provider().is_none() {
        return Ok(None);
    }
    // An inode without attached runtime user data has no fileattr gate.
    // That is not an error: the setter facade cannot route an attr update to
    // such an inode either (it fails the same lookup), so no attr mutation
    // can ever race this writeback and there is nothing to serialize with.
    let Some(data) = location.entry().persistent_user_data() else {
        return Ok(None);
    };
    Ok(Some(data.begin_file_attr_mutation()))
}

/// Private witness for cache writeback helpers whose caller already owns the
/// inode's native fileattr gate.  It has no public constructor: callers must
/// derive it from the guard that remains live over the writeback operation.
#[derive(Clone, Copy)]
pub(super) struct HeldNativeWritebackGate<'a>(
    pub(super) core::marker::PhantomData<&'a Option<FileAttrMutationGuard>>,
);

pub(super) fn held_native_writeback_gate(
    _guard: &Option<FileAttrMutationGuard>,
) -> HeldNativeWritebackGate<'_> {
    HeldNativeWritebackGate(core::marker::PhantomData)
}

impl Drop for NowaitWritebackAnchorGuard {
    fn drop(&mut self) {
        if self.committed || !self.installed {
            return;
        }
        // An immediate path must not turn rollback into a wait.  A contended
        // registry cannot make the dirty-page owner unsafe; retain it rather
        // than block.  The normal clean-release path will reclaim it.
        let Some(mut registry) = file_cache_registry().try_lock() else {
            return;
        };
        let Some(entry) = registry.get_mut(&self.key) else {
            return;
        };
        if entry
            .shared()
            .is_some_and(|registered| Arc::ptr_eq(&registered, &self.shared))
        {
            entry.writeback_anchor = None;
        }
    }
}

pub(super) fn try_retain_cached_file_writeback_anchor(
    location: &Location,
    shared: &Arc<CachedFileShared>,
) -> Option<NowaitWritebackAnchorGuard> {
    let key = cached_file_registry_key(location);
    let Some(mut registry) = file_cache_registry().try_lock() else {
        return None;
    };
    let Some(entry) = registry.get_mut(&key) else {
        return None;
    };
    if !entry
        .shared()
        .is_some_and(|registered| Arc::ptr_eq(&registered, shared))
    {
        return None;
    }
    entry.update_location(location);
    let mut installed = false;
    if entry.writeback_anchor.is_none() {
        entry.writeback_anchor = Some(location.writeback_anchor());
        installed = true;
    }
    Some(NowaitWritebackAnchorGuard {
        key,
        shared: shared.clone(),
        installed,
        committed: false,
    })
}

pub(super) fn release_cached_file_writeback_anchor_if_clean(shared: &CachedFileShared) {
    let page_cache = shared.page_cache.lock();
    if page_cache.iter().any(|(_pn, page)| page.is_dirty()) {
        return;
    }

    let released = {
        let mut registry = file_cache_registry().lock();
        registry
            .values_mut()
            .filter(|entry| {
                entry
                    .shared()
                    .is_some_and(|registered| core::ptr::eq(Arc::as_ptr(&registered), shared))
            })
            .filter_map(|entry| entry.writeback_anchor.take())
            .collect::<Vec<_>>()
    };
    drop(page_cache);
    drop(released);
}

// Cache-invalidation machinery for the in-progress write path.
#[allow(dead_code)]
pub(super) fn release_unlinked_cached_file_registry_ownership(
    location: &Location,
    shared: &Arc<CachedFileShared>,
) {
    let key = cached_file_registry_key(location);
    let retired = {
        let mut registry = file_cache_registry().lock();
        let matches_shared = registry
            .get(&key)
            .is_some_and(|entry| entry.references_shared(shared));
        matches_shared.then(|| registry.remove(&key)).flatten()
    };
    // Retention and writeback anchors can own filesystem or inode state whose
    // teardown re-enters cache bookkeeping.
    drop(retired);
}

/// Removes the registry ownership after an unlinked cache has been discarded.
///
/// This variant is used by a range-lease drop, where retaining the original
/// `Location` would defeat the purpose of making the last lease the cleanup
/// trigger. The shared identity is the same generation-checked key used by
/// the location-based helper above.
pub(super) fn release_unlinked_cached_file_registry_ownership_for_shared(
    shared: &Arc<CachedFileShared>,
) {
    let retired = {
        let mut registry = file_cache_registry().lock();
        let matches_shared = registry
            .get(&shared.registry_key)
            .is_some_and(|entry| entry.references_shared(shared));
        matches_shared
            .then(|| registry.remove(&shared.registry_key))
            .flatten()
    };
    drop(retired);
}

pub(super) type CachedFileWritebackSnapshotEntry = (
    CachedFileRegistryKey,
    Arc<CachedFileShared>,
    WritebackAnchor,
);

pub(super) fn cached_file_writeback_snapshot() -> Vec<CachedFileWritebackSnapshotEntry> {
    let (entries, deferred, retired) = {
        let mut registry = file_cache_registry().lock();
        let mut entries = Vec::new();
        let mut dead_keys = Vec::new();
        let mut deferred = Vec::new();

        for (key, entry) in registry.iter() {
            let shared = entry.shared();
            let anchor = entry.writeback_anchor();
            match (shared, anchor) {
                (Some(shared), Some(anchor)) => entries.push((*key, shared, anchor)),
                (shared, anchor) => {
                    dead_keys.push(*key);
                    deferred.push((shared, anchor));
                }
            }
        }

        let retired = dead_keys
            .into_iter()
            .filter_map(|key| registry.remove(&key))
            .collect::<Vec<_>>();
        (entries, deferred, retired)
    };

    // Snapshot failures and dead registry entries can own filesystem state.
    // Their destruction must happen only after the registry guard is gone.
    drop(deferred);
    drop(retired);
    entries
}

/// Drops the shared page-cache registry entry for a fully released inode.
pub fn remove_cached_file_registry_entry(device: u64, inode: u64) {
    let retired = {
        let mut registry = file_cache_registry().lock();
        // This legacy raw-slot cleanup has no generation token.  Never
        // remove a live entry: an old inode generation may be finishing
        // after a replacement has already occupied the same device/inode
        // slot.  The exact identity path removes live entries on last close;
        // the caller follows this helper with the inode-scoped dead-entry
        // prune.
        let key = registry.iter().find_map(|(key, entry)| {
            (key.device() == device && key.inode() == inode && !entry.has_live_shared())
                .then_some(*key)
        });
        key.and_then(|key| registry.remove(&key))
    };
    drop(retired);
}

/// Prunes dead cache registry entries for a released inode.
pub fn prune_dead_cached_file_registry_entries_for_inode(inode: u64) {
    let retired = {
        let mut registry = file_cache_registry().lock();
        let dead_keys = registry
            .iter()
            .filter_map(|(key, entry)| {
                (key.inode() == inode && !entry.has_live_shared()).then_some(*key)
            })
            .collect::<Vec<_>>();
        dead_keys
            .into_iter()
            .filter_map(|key| registry.remove(&key))
            .collect::<Vec<_>>()
    };
    drop(retired);
}

pub(super) fn cached_file_page_count(shared: &CachedFileShared) -> usize {
    shared.page_cache.lock().len()
}

/// Conservative, bounded snapshot used to estimate immediately reclaimable
/// file-cache memory.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachedFileReclaimEstimate {
    /// Clean, unpinned pages observed in ordinary disk-backed caches.
    pub reclaimable_pages: usize,
    /// Registry entries whose cache state was inspected.
    pub scanned_files: usize,
    /// Entries skipped because their page-cache lock was contended.
    pub busy_files: usize,
    /// Entries skipped because they have active mapping listeners.
    pub mapped_files: usize,
    /// Some live registry entries were outside the bounded snapshot.
    pub snapshot_truncated: bool,
}

/// Outcome of one bounded, non-blocking global clean-cache reclaim pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachedFileReclaimStats {
    pub requested_pages: usize,
    /// Registry slots visited, including dead or unsupported entries.
    pub visited_registry_entries: usize,
    /// Registry size observed when this bounded snapshot was taken.
    pub registry_entries: usize,
    pub scanned_files: usize,
    pub scanned_pages: usize,
    pub reclaimed_pages: usize,
    pub dirty_pages: usize,
    pub pinned_pages: usize,
    pub writeback_pages: usize,
    pub busy_files: usize,
    pub mapped_files: usize,
    pub scan_budget_exhausted_files: usize,
    pub snapshot_truncated: bool,
}

pub(super) struct CachedFileReclaimSnapshot {
    pub(super) entries: [Option<Arc<CachedFileShared>>; GLOBAL_FILE_CACHE_SCAN_LIMIT],
    pub(super) len: usize,
    pub(super) visited: usize,
    pub(super) registry_entries: usize,
    pub(super) truncated: bool,
}

pub(super) fn cached_file_reclaim_snapshot(
    cursor: &Mutex<Option<CachedFileRegistryKey>>,
) -> CachedFileReclaimSnapshot {
    let registry = file_cache_registry().lock();
    let mut snapshot = CachedFileReclaimSnapshot {
        entries: core::array::from_fn(|_| None),
        len: 0,
        visited: 0,
        registry_entries: registry.len(),
        truncated: false,
    };
    let mut cursor = cursor.lock();
    let mut visited = 0usize;
    let mut last_visited = None;
    let start = *cursor;

    macro_rules! visit_entries {
        ($entries:expr) => {
            for (key, entry) in $entries {
                if visited == GLOBAL_FILE_CACHE_SCAN_LIMIT {
                    break;
                }
                visited += 1;
                last_visited = Some(*key);
                if let Some(shared) = entry.shared() {
                    snapshot.entries[snapshot.len] = Some(shared);
                    snapshot.len += 1;
                }
            }
        };
    }

    if let Some(start) = start {
        visit_entries!(registry.range((
            core::ops::Bound::Excluded(start),
            core::ops::Bound::Unbounded,
        )));
        if visited < GLOBAL_FILE_CACHE_SCAN_LIMIT {
            visit_entries!(registry.range(..=start));
        }
    } else {
        visit_entries!(registry.iter());
    }
    if let Some(last_visited) = last_visited {
        *cursor = Some(last_visited);
    }
    snapshot.truncated = visited < registry.len();
    snapshot.visited = visited;
    drop(cursor);
    drop(registry);
    snapshot
}

/// Returns a bounded lower-bound estimate of clean file-cache pages that can
/// be reclaimed without writeback.  tmpfs/ALWAYS_CACHE pages, dirty pages,
/// pinned pages, writeback pages, and contended caches are deliberately not
/// counted.
pub fn cached_file_reclaim_estimate() -> CachedFileReclaimEstimate {
    let snapshot = cached_file_reclaim_snapshot(file_cache_estimate_cursor());
    let mut estimate = CachedFileReclaimEstimate {
        snapshot_truncated: snapshot.truncated,
        ..CachedFileReclaimEstimate::default()
    };
    for shared in snapshot.entries.into_iter().take(snapshot.len).flatten() {
        if shared.in_memory {
            continue;
        }
        let Some(_direct_guard) = shared.direct_io_lock.try_lock() else {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        };
        let Some(admission) = shared.user_io_pin_admission.try_lock() else {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        };
        if admission.invalidating || admission.cache_users != 0 || admission.pin_windows != 0 {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        }
        let Some(_writeback_guard) = shared.writeback_lock.try_write() else {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        };
        let Some(listeners) = shared.evict_listeners.try_lock() else {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        };
        if !listeners.is_empty() {
            estimate.mapped_files = estimate.mapped_files.saturating_add(1);
            continue;
        }
        let Some(cache) = shared.page_cache.try_lock() else {
            estimate.busy_files = estimate.busy_files.saturating_add(1);
            continue;
        };
        estimate.scanned_files = estimate.scanned_files.saturating_add(1);
        estimate.reclaimable_pages = estimate.reclaimable_pages.saturating_add(
            cache
                .iter()
                .take(GLOBAL_FILE_CACHE_ESTIMATE_PER_FILE)
                .filter(|(_pn, page)| !page.is_dirty() && !page.is_pinned() && !page.is_writeback())
                .count(),
        );
    }
    estimate
}

/// Reconciles closed-cache accounting with the page-cache state observed
/// while the caller holds `direct_io_lock` for writing.
///
/// Using the actual remaining page count is important: the last open handle
/// may establish retention immediately before a pressure pass.  Applying a
/// reclaim delta to that newer retention would otherwise be able to release a
/// shared cache that still contains dirty pages.
pub(super) fn synchronize_retained_page_count(
    shared: &Arc<CachedFileShared>,
    remaining_pages: usize,
) {
    let released = {
        let mut registry = file_cache_registry().lock();
        let Some(entry) = registry.get_mut(&shared.registry_key) else {
            return;
        };
        let is_retained_shared = entry
            .retained
            .as_ref()
            .is_some_and(|retained| Arc::ptr_eq(retained, shared));
        if !is_retained_shared {
            return;
        }

        let old_pages = entry.retained_pages;
        if remaining_pages > old_pages {
            CLOSED_FILE_CACHE_RETAINED_PAGES
                .fetch_add(remaining_pages - old_pages, Ordering::AcqRel);
        } else if old_pages > remaining_pages {
            CLOSED_FILE_CACHE_RETAINED_PAGES
                .fetch_sub(old_pages - remaining_pages, Ordering::AcqRel);
        }
        entry.retained_pages = remaining_pages;
        if remaining_pages == 0 {
            entry.release_retained()
        } else {
            None
        }
    };
    drop(released);
}

pub(super) fn reclaim_clean_pages_from_shared(
    shared: &Arc<CachedFileShared>,
    target_pages: usize,
    stats: &mut CachedFileReclaimStats,
) -> usize {
    reclaim_clean_pages_from_shared_with_scan_budget(
        shared,
        target_pages,
        GLOBAL_FILE_CACHE_RECLAIM_SCAN_PER_FILE,
        stats,
    )
}

pub(super) fn reclaim_clean_pages_from_shared_with_scan_budget(
    shared: &Arc<CachedFileShared>,
    target_pages: usize,
    scan_budget_per_pass: usize,
    stats: &mut CachedFileReclaimStats,
) -> usize {
    if target_pages == 0 || shared.in_memory {
        return 0;
    }

    let reclaimed = {
        let Some(_direct_guard) = shared.direct_io_lock.try_lock() else {
            stats.busy_files = stats.busy_files.saturating_add(1);
            return 0;
        };
        let Ok(_mutation) = CachedFile::try_begin_shared_cache_reclaim(shared) else {
            stats.busy_files = stats.busy_files.saturating_add(1);
            return 0;
        };
        let Some(_writeback_guard) = shared.writeback_lock.try_write() else {
            stats.busy_files = stats.busy_files.saturating_add(1);
            return 0;
        };
        let Some(listeners) = shared.evict_listeners.try_lock() else {
            stats.busy_files = stats.busy_files.saturating_add(1);
            return 0;
        };
        if !listeners.is_empty() {
            // A mapped page needs PTE teardown and a TLB grace period.  The
            // first pressure slice deliberately leaves that work to a future
            // batched interface instead of issuing one shootdown per page.
            stats.mapped_files = stats.mapped_files.saturating_add(1);
            return 0;
        }

        let scan_epoch = FILE_CACHE_RECLAIM_SCAN_EPOCH.load(Ordering::Acquire);
        let inode_scan_epoch = shared.pressure_reclaim_scan_epoch.load(Ordering::Acquire);
        let mut cycle_scan_remaining = shared
            .pressure_reclaim_scan_remaining
            .load(Ordering::Acquire);
        if inode_scan_epoch == scan_epoch && cycle_scan_remaining == 0 {
            // This inode has already completed a full LRU traversal in the
            // current system-wide epoch. Do not silently start another cycle:
            // different inode sizes would otherwise have to align before the
            // worker could ever observe one complete no-progress sweep.
            return 0;
        }

        let mut reclaimed = 0usize;
        let mut remaining_pages_after_reclaim = None;
        let mut cycle_scan_initialized = inode_scan_epoch == scan_epoch;
        let mut remaining_scan_budget = scan_budget_per_pass;
        while reclaimed < target_pages && remaining_scan_budget != 0 {
            // Take a one-page publication credit before taking the page-cache
            // lock.  This preserves the cache->shadow lock ordering used by
            // cachestat and, more importantly, proves the later detach has a
            // no-fail shadow commit.  A scan with no candidate simply drops
            // this credit at the end of the iteration.
            let shadow_reservation = match prepare_file_cache_shadow_publication(shared) {
                Ok(reservation) => reservation,
                Err(VfsError::NoMemory) => break,
                Err(_) => unreachable!("shadow preparation only reports ENOMEM"),
            };
            let (candidate, remaining_pages) = {
                let Some(mut cache) = shared.page_cache.try_lock() else {
                    stats.busy_files = stats.busy_files.saturating_add(1);
                    break;
                };
                if !cycle_scan_initialized {
                    cycle_scan_remaining = cache.len();
                    shared
                        .pressure_reclaim_scan_remaining
                        .store(cycle_scan_remaining, Ordering::Relaxed);
                    shared
                        .pressure_reclaim_scan_epoch
                        .store(scan_epoch, Ordering::Release);
                    cycle_scan_initialized = true;
                } else {
                    cycle_scan_remaining = cycle_scan_remaining.min(cache.len());
                }
                let scan = pop_clean_unpinned_lru_page(
                    &mut cache,
                    remaining_scan_budget.min(cycle_scan_remaining),
                );
                remaining_scan_budget = remaining_scan_budget.saturating_sub(scan.scanned);
                cycle_scan_remaining = cycle_scan_remaining.saturating_sub(scan.scanned);
                shared
                    .pressure_reclaim_scan_remaining
                    .store(cycle_scan_remaining, Ordering::Release);
                stats.scanned_pages = stats.scanned_pages.saturating_add(scan.scanned);
                stats.dirty_pages = stats.dirty_pages.saturating_add(scan.dirty);
                stats.pinned_pages = stats.pinned_pages.saturating_add(scan.pinned);
                stats.writeback_pages = stats.writeback_pages.saturating_add(scan.writeback);
                (scan.page, cache.len())
            };
            remaining_pages_after_reclaim = Some(remaining_pages);
            let Some((pn, page)) = candidate else {
                break;
            };
            // The listener lock is still held and known empty, so no mapping
            // can begin observing this cache page before it is released.
            record_prepared_file_cache_shadow(shadow_reservation, shared, pn);
            drop(page);
            reclaimed += 1;
        }
        if reclaimed < target_pages && remaining_scan_budget == 0 && cycle_scan_remaining != 0 {
            stats.scan_budget_exhausted_files = stats.scan_budget_exhausted_files.saturating_add(1);
        }

        if let (true, Some(remaining_pages)) = (reclaimed != 0, remaining_pages_after_reclaim) {
            synchronize_retained_page_count(shared, remaining_pages);
        }
        reclaimed
    };

    reclaimed
}

/// Reclaims at most a fixed system-wide batch of clean, unpinned,
/// non-writeback pages from ordinary disk-backed files.  The operation never
/// waits for a contended cache/direct-I/O path and never initiates writeback.
pub fn reclaim_clean_cached_file_pages(target_pages: usize) -> CachedFileReclaimStats {
    let bounded_target = target_pages
        .min(GLOBAL_FILE_CACHE_SCAN_LIMIT.saturating_mul(GLOBAL_FILE_CACHE_RECLAIM_PER_FILE));
    let mut stats = CachedFileReclaimStats {
        requested_pages: bounded_target,
        ..CachedFileReclaimStats::default()
    };
    if bounded_target == 0 {
        return stats;
    }
    let snapshot = cached_file_reclaim_snapshot(file_cache_reclaim_cursor());
    stats.visited_registry_entries = snapshot.visited;
    stats.registry_entries = snapshot.registry_entries;
    stats.snapshot_truncated = snapshot.truncated;
    for shared in snapshot.entries.into_iter().take(snapshot.len).flatten() {
        if stats.reclaimed_pages == bounded_target {
            break;
        }
        stats.scanned_files = stats.scanned_files.saturating_add(1);
        let per_file_target = bounded_target
            .saturating_sub(stats.reclaimed_pages)
            .min(GLOBAL_FILE_CACHE_RECLAIM_PER_FILE);
        stats.reclaimed_pages =
            stats
                .reclaimed_pages
                .saturating_add(reclaim_clean_pages_from_shared(
                    &shared,
                    per_file_target,
                    &mut stats,
                ));
    }
    stats
}

/// Starts a new system-wide bounded LRU scan after the previous epoch reached
/// a complete no-progress registry sweep. Individual inode cursors are reset
/// lazily, so advancing the epoch is constant-time and allocation-free.
pub fn advance_clean_cached_file_reclaim_scan_epoch() {
    FILE_CACHE_RECLAIM_SCAN_EPOCH
        .try_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
            epoch.checked_add(1)
        })
        .expect("clean file-cache reclaim scan epoch exhausted");
}

pub(super) fn release_closed_cached_file_retention(location: &Location) {
    let key = cached_file_registry_key(location);
    let released = {
        let mut registry = file_cache_registry().lock();
        registry
            .get_mut(&key)
            .and_then(FileUserData::release_retained)
    };
    drop(released);
}

pub(super) struct ClosedFileCacheTrimCandidate {
    pub(super) key: CachedFileRegistryKey,
    pub(super) shared: Arc<CachedFileShared>,
    pub(super) anchor: WritebackAnchor,
    pub(super) pages: usize,
    pub(super) epoch: u64,
}

pub(super) enum ClosedFileCacheRetentionDecision {
    Retained(Option<Arc<CachedFileShared>>),
    Trim(Vec<ClosedFileCacheTrimCandidate>),
}

pub(super) fn closed_file_cache_trim_candidates(
    registry: &BTreeMap<CachedFileRegistryKey, FileUserData>,
    preserve_key: CachedFileRegistryKey,
    current_retained_pages: usize,
    required_pages: usize,
) -> Vec<ClosedFileCacheTrimCandidate> {
    if current_retained_pages.saturating_add(required_pages) <= CLOSED_FILE_CACHE_RETAIN_MAX_PAGES {
        return Vec::new();
    }

    let mut candidates = registry
        .iter()
        .filter_map(|(key, entry)| {
            if *key == preserve_key || entry.retained_pages == 0 {
                return None;
            }
            let shared = entry.retained.as_ref()?.clone();
            if shared.open_handles.load(Ordering::Acquire) != 0 {
                return None;
            }
            Some(ClosedFileCacheTrimCandidate {
                key: *key,
                shared,
                anchor: entry.writeback_anchor()?,
                pages: entry.retained_pages,
                epoch: entry.retained_epoch,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|candidate| candidate.epoch);

    let mut projected = current_retained_pages;
    let mut selected = Vec::new();
    for candidate in candidates {
        projected = projected.saturating_sub(candidate.pages);
        selected.push(candidate);
        if projected.saturating_add(required_pages) <= CLOSED_FILE_CACHE_RETAIN_MAX_PAGES {
            break;
        }
    }
    selected
}

pub(super) fn flush_and_release_closed_file_cache_candidate(
    candidate: ClosedFileCacheTrimCandidate,
) -> bool {
    if candidate.shared.open_handles.load(Ordering::Acquire) != 0 {
        return false;
    }

    let file = match candidate.anchor.entry().as_file() {
        Ok(file) => file,
        Err(err) => {
            debug!("Failed to access retained cached file for trim: {err:?}");
            record_cached_file_counter(&CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS, 1);
            return false;
        }
    };
    if let Err(err) = flush_dirty_cache_shared(&candidate.shared, file) {
        ratelimit::warn_ratelimited!("Failed to flush retained cached file before trim: {err:?}");
        record_cached_file_counter(&CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS, 1);
        return false;
    }
    if let Err(err) = candidate.anchor.flush_metadata_time_overlay() {
        ratelimit::warn_ratelimited!(
            "Failed to flush retained cached file timestamps before trim: {err:?}"
        );
        record_cached_file_counter(&CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS, 1);
        return false;
    }
    if let Err(err) = discard_cached_pages(&candidate.shared) {
        ratelimit::warn_ratelimited!(
            "Failed to invalidate retained cached file before trim: {err:?}"
        );
        record_cached_file_counter(&CLOSED_FILE_CACHE_TRIM_FLUSH_ERRORS, 1);
        return false;
    }

    let released = {
        let mut registry = file_cache_registry().lock();
        let Some(entry) = registry.get_mut(&candidate.key) else {
            return false;
        };
        let still_retained = entry
            .retained
            .as_ref()
            .is_some_and(|shared| Arc::ptr_eq(shared, &candidate.shared));
        if !still_retained || candidate.shared.open_handles.load(Ordering::Acquire) != 0 {
            return false;
        }
        let pages = entry.retained_pages;
        entry.release_retained().map(|retained| (retained, pages))
    };
    if let Some((retained, pages)) = released {
        record_cached_file_counter(&CLOSED_FILE_CACHE_TRIM_RELEASES, 1);
        record_cached_file_counter(&CLOSED_FILE_CACHE_TRIM_PAGES, pages as u64);
        drop(retained);
        return true;
    }
    false
}

pub(super) fn try_retain_closed_cached_file(
    location: &Location,
    shared: &Arc<CachedFileShared>,
) -> bool {
    if cached_file_is_in_memory(location) || shared.unlinked.load(Ordering::Acquire) {
        return false;
    }
    record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_ATTEMPTS, 1);
    if shared.open_handles.load(Ordering::Acquire) != 0 {
        return false;
    }

    let key = cached_file_registry_key(location);
    loop {
        let (pages, decision) = {
            // Pressure reclaim owns this lock for writing.  Keep the page
            // count and retention publication in one read-side transaction,
            // so a reclaim pass can only run wholly before or wholly after it.
            let _direct_guard = shared.direct_io_lock.lock();
            if shared.open_handles.load(Ordering::Acquire) != 0 {
                return false;
            }
            let pages = cached_file_page_count(shared);
            if pages == 0 {
                return false;
            }
            release_cached_file_writeback_anchor_if_clean(shared);

            let mut registry = file_cache_registry().lock();
            if shared.open_handles.load(Ordering::Acquire) != 0 {
                return false;
            }
            let entry = registry
                .entry(key)
                .or_insert_with(|| FileUserData::new(location, shared));
            let current_without_entry = CLOSED_FILE_CACHE_RETAINED_PAGES
                .load(Ordering::Acquire)
                .saturating_sub(entry.retained_pages);
            if current_without_entry.saturating_add(pages) <= CLOSED_FILE_CACHE_RETAIN_MAX_PAGES {
                let retired = entry.retain_closed(location, shared, pages);
                (pages, ClosedFileCacheRetentionDecision::Retained(retired))
            } else {
                (
                    pages,
                    ClosedFileCacheRetentionDecision::Trim(closed_file_cache_trim_candidates(
                        &registry,
                        key,
                        current_without_entry,
                        pages,
                    )),
                )
            }
        };

        let trim_candidates = match decision {
            ClosedFileCacheRetentionDecision::Retained(retired) => {
                // Replacing a retained cache can release filesystem-backed
                // state. Keep that destructor outside the registry lock.
                drop(retired);
                record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_HITS, 1);
                record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_PAGES, pages as u64);
                return true;
            }
            ClosedFileCacheRetentionDecision::Trim(candidates) => candidates,
        };

        if trim_candidates.is_empty() {
            record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_REJECT_PAGES, pages as u64);
            return false;
        }

        let mut trimmed = false;
        for candidate in trim_candidates {
            trimmed |= flush_and_release_closed_file_cache_candidate(candidate);
        }
        if !trimmed {
            record_cached_file_counter(&CLOSED_FILE_CACHE_RETAIN_REJECT_PAGES, pages as u64);
            return false;
        }
    }
}
