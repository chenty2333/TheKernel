//! Cache invalidation around file operations, LRU scans and filesystem-wide sync.

use super::*;

// Cache-invalidation machinery for the in-progress write path.
#[allow(dead_code)]
pub(super) fn sync_and_invalidate_cached_file_pages_locked(
    location: &Location,
    shared: &Arc<CachedFileShared>,
    mutation: &CachedFileMutationGuard,
) -> VfsResult<()> {
    let _native_mutation = begin_source_location_writeback_mutation(location)?;
    sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
        location,
        shared,
        mutation,
        held_native_writeback_gate(&_native_mutation),
    )
}

/// Variant for a caller which already owns that inode's fileattr mutation
/// gate.  Re-entering the non-reentrant gate from page writeback would turn a
/// deterministic multi-inode transaction into a self-deadlock.
pub(super) fn sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
    location: &Location,
    shared: &Arc<CachedFileShared>,
    mutation: &CachedFileMutationGuard,
    native_gate: HeldNativeWritebackGate<'_>,
) -> VfsResult<()> {
    let _writeback_guard = shared.writeback_lock.write();
    wait_for_all_writeback_clear(shared);
    let file = location.entry().as_file()?;
    let mut invalidation = CachedPageInvalidationTransaction::new(mutation);
    invalidation.stage_all()?;
    invalidation.prepare_evictions()?;
    invalidation.writeback_with_held_native_gate(file, false, native_gate)?;
    invalidation.commit_discard();
    release_cached_file_writeback_anchor_if_clean(shared);
    release_closed_cached_file_retention(location);
    Ok(())
}

pub(super) fn with_cache_invalidating_file_operation<R>(
    location: &Location,
    operation: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<R>,
) -> VfsResult<R> {
    // This public/default admission owns the native gate before direct-I/O
    // exclusion.  Callers which already own that gate must use the explicitly
    // named held-gate helper below instead of recursively acquiring it.
    let native_mutation = begin_source_location_writeback_mutation(location)?;
    with_cache_invalidating_file_operation_with_held_native(
        location,
        held_native_writeback_gate(&native_mutation),
        operation,
    )
}

/// Cache-invalidating direct operation for a caller which retains the inode's
/// native admission across the complete provider and post-invalidation
/// window.  This must never acquire the native gate itself.
pub(super) fn with_cache_invalidating_file_operation_with_held_native<R>(
    location: &Location,
    native_gate: HeldNativeWritebackGate<'_>,
    operation: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<R>,
) -> VfsResult<R> {
    let shared = cached_file_shared_for_location_or_create(location);
    let _direct_guard = shared.direct_io_lock.lock();
    let mutation = CachedFile::begin_shared_cache_invalidating_mutation(&shared)?;
    sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
        location,
        &shared,
        &mutation,
        native_gate,
    )?;
    let file = location.entry().as_file()?;
    let result = operation(&shared, file);
    let post_result = sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
        location,
        &shared,
        &mutation,
        native_gate,
    );
    if let Err(error) = post_result {
        return Err(error);
    }
    result
}

/// Runs an extent-sharing operation after the source has been made backend
/// coherent and the destination has entered the normal invalidating mutation
/// domain.  A reflink/dedupe provider reads source extents from its backend;
/// using only the destination transaction would let it observe stale data
/// still dirty in the source page cache.
///
/// Distinct objects acquire native fileattr gates and then direct-I/O gates in
/// the same stable ObjectKey order.  The source remains stable from dirty
/// writeback through provider commit; the same-object path owns only one
/// mutation/gate and therefore cannot lock itself recursively.
pub(super) fn with_source_coherent_destination_invalidated<R>(
    source: &Location,
    destination: &Location,
    operation: impl FnOnce(&FileNode, &FileNode) -> VfsResult<R>,
) -> VfsResult<R> {
    let source_file = source.entry().as_file()?;
    let destination_file = destination.entry().as_file()?;
    let source_shared = cached_file_shared_for_location_or_create(source);
    let destination_shared = cached_file_shared_for_location_or_create(destination);
    let source_key = source.object_key();
    let destination_key = destination.object_key();

    if source_key == destination_key && Arc::ptr_eq(&source_shared, &destination_shared) {
        // Self-reflink is a destination mutation, so one checked native gate
        // covers both roles before taking the matching direct-I/O gate.
        let destination_native_mutation = begin_native_location_mutation(destination, false)?;
        let _direct_guard = source_shared.direct_io_lock.lock();
        return with_source_coherent_destination_invalidated_locked(
            source,
            destination,
            &source_shared,
            &destination_shared,
            source_file,
            destination_file,
            held_native_writeback_gate(&destination_native_mutation),
            held_native_writeback_gate(&destination_native_mutation),
            true,
            operation,
        );
    }

    if source_key == destination_key {
        // Different cache-sharing allocations may still name one backend
        // inode (for example a hard-link/per-dentry fallback).  Its native
        // fileattr gate is therefore acquired exactly once, while both cache
        // domains receive independently ordered direct-I/O gates.
        let destination_native_mutation = begin_native_location_mutation(destination, false)?;
        let source_address = Arc::as_ptr(&source_shared) as usize;
        let destination_address = Arc::as_ptr(&destination_shared) as usize;
        if source_address < destination_address {
            let _source_direct_guard = source_shared.direct_io_lock.lock();
            let _destination_direct_guard = destination_shared.direct_io_lock.lock();
            return with_source_coherent_destination_invalidated_locked(
                source,
                destination,
                &source_shared,
                &destination_shared,
                source_file,
                destination_file,
                held_native_writeback_gate(&destination_native_mutation),
                held_native_writeback_gate(&destination_native_mutation),
                false,
                operation,
            );
        }
        let _destination_direct_guard = destination_shared.direct_io_lock.lock();
        let _source_direct_guard = source_shared.direct_io_lock.lock();
        return with_source_coherent_destination_invalidated_locked(
            source,
            destination,
            &source_shared,
            &destination_shared,
            source_file,
            destination_file,
            held_native_writeback_gate(&destination_native_mutation),
            held_native_writeback_gate(&destination_native_mutation),
            false,
            operation,
        );
    }

    let source_order = (
        source_key.filesystem,
        source_key.object,
        source_key.generation,
    );
    let destination_order = (
        destination_key.filesystem,
        destination_key.object,
        destination_key.generation,
    );
    if source_order < destination_order {
        let source_native_mutation = begin_source_location_writeback_mutation(source)?;
        let destination_native_mutation = begin_native_location_mutation(destination, false)?;
        let _source_direct_guard = source_shared.direct_io_lock.lock();
        let _destination_direct_guard = destination_shared.direct_io_lock.lock();
        with_source_coherent_destination_invalidated_locked(
            source,
            destination,
            &source_shared,
            &destination_shared,
            source_file,
            destination_file,
            held_native_writeback_gate(&source_native_mutation),
            held_native_writeback_gate(&destination_native_mutation),
            false,
            operation,
        )
    } else {
        let destination_native_mutation = begin_native_location_mutation(destination, false)?;
        let source_native_mutation = begin_source_location_writeback_mutation(source)?;
        let _destination_direct_guard = destination_shared.direct_io_lock.lock();
        let _source_direct_guard = source_shared.direct_io_lock.lock();
        with_source_coherent_destination_invalidated_locked(
            source,
            destination,
            &source_shared,
            &destination_shared,
            source_file,
            destination_file,
            held_native_writeback_gate(&source_native_mutation),
            held_native_writeback_gate(&destination_native_mutation),
            false,
            operation,
        )
    }
}

pub(super) fn with_source_coherent_destination_invalidated_locked<R>(
    source: &Location,
    destination: &Location,
    source_shared: &Arc<CachedFileShared>,
    destination_shared: &Arc<CachedFileShared>,
    source_file: &FileNode,
    destination_file: &FileNode,
    source_native_gate: HeldNativeWritebackGate<'_>,
    destination_native_gate: HeldNativeWritebackGate<'_>,
    same_object: bool,
    operation: impl FnOnce(&FileNode, &FileNode) -> VfsResult<R>,
) -> VfsResult<R> {
    if same_object {
        // A self-reflink/dedupe reads and mutates the same backend object, so
        // it needs the ordinary invalidation transaction rather than the
        // distinct-source clean-cache fast path below.
        let source_mutation = CachedFile::begin_shared_cache_invalidating_mutation(source_shared)?;
        sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
            source,
            source_shared,
            &source_mutation,
            source_native_gate,
        )?;
        let result = operation(source_file, destination_file);
        let post_result = sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
            destination,
            source_shared,
            &source_mutation,
            destination_native_gate,
        );
        return match post_result {
            Ok(()) => result,
            Err(error) => Err(error),
        };
    }

    // Both native gates and both direct-I/O gates are already held in the
    // same ObjectKey order.  Source writeback therefore cannot race an attr
    // setter or a new cached/direct write before the provider observes it.
    let source_mutation = CachedFile::begin_shared_cache_invalidating_mutation(source_shared)?;
    sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
        source,
        source_shared,
        &source_mutation,
        source_native_gate,
    )?;
    let destination_mutation =
        CachedFile::begin_shared_cache_invalidating_mutation(destination_shared)?;
    sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
        destination,
        destination_shared,
        &destination_mutation,
        destination_native_gate,
    )?;
    let result = operation(source_file, destination_file);
    let post_result = sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
        destination,
        destination_shared,
        &destination_mutation,
        destination_native_gate,
    );
    match post_result {
        Ok(()) => result,
        Err(error) => Err(error),
    }
}

/// Runs a direct operation only after a lower filesystem has reported that
/// the exact request is executable.  The preflight and the later operation
/// share one direct-I/O write lock, so an extent/EOF capability decision cannot
/// go stale while cached pages are being invalidated.  In particular, a
/// rejected hole, fragmented run, or EOF request leaves the cache and file
/// untouched; a lower layer may also reject an unavailable device before
/// execution when it can prove that no descriptor was published.
// Cache-invalidation machinery for the in-progress write path.
#[allow(dead_code)]
pub(super) fn with_cache_invalidating_file_operation_after_preflight<R>(
    location: &Location,
    preflight: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<bool>,
    operation: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<R>,
) -> VfsResult<Option<R>> {
    let native_mutation = begin_source_location_writeback_mutation(location)?;
    with_cache_invalidating_file_operation_after_preflight_with_held_native(
        location,
        held_native_writeback_gate(&native_mutation),
        preflight,
        operation,
    )
}

// Cache-invalidation machinery for the in-progress write path.
#[allow(dead_code)]
pub(super) fn with_cache_invalidating_file_operation_after_preflight_with_held_native<R>(
    location: &Location,
    native_gate: HeldNativeWritebackGate<'_>,
    preflight: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<bool>,
    operation: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<R>,
) -> VfsResult<Option<R>> {
    let shared = cached_file_shared_for_location_or_create(location);
    let _direct_guard = shared.direct_io_lock.lock();
    let file = location.entry().as_file()?;
    if !preflight(&shared, file)? {
        return Ok(None);
    }
    let mutation = CachedFile::begin_shared_cache_invalidating_mutation(&shared)?;
    sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
        location,
        &shared,
        &mutation,
        native_gate,
    )?;
    let file = location.entry().as_file()?;
    let result = operation(&shared, file);
    let post_result = sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
        location,
        &shared,
        &mutation,
        native_gate,
    );
    if let Err(error) = post_result {
        return Err(error);
    }
    result.map(Some)
}

/// Acquires a direct range lease, stages overlapping cache pages, and only
/// then runs the filesystem preflight. Once the preflight accepts, all cache
/// locks and staged-page bookkeeping are released before the lower operation
/// can wait on a synchronous device; the owned range lease remains the sole
/// exclusion token until the operation has revalidated its mapping.
pub(super) fn with_direct_range_operation_after_preflight<R>(
    location: &Location,
    offset: u64,
    len: usize,
    kind: RangeCacheLeaseKind,
    preflight: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<bool>,
    operation: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<R>,
) -> VfsResult<Option<R>> {
    let native_mutation = begin_source_location_writeback_mutation(location)?;
    with_direct_range_operation_after_preflight_with_held_native(
        location,
        offset,
        len,
        kind,
        held_native_writeback_gate(&native_mutation),
        preflight,
        operation,
    )
}

pub(super) fn with_direct_range_operation_after_preflight_with_held_native<R>(
    location: &Location,
    offset: u64,
    len: usize,
    kind: RangeCacheLeaseKind,
    native_gate: HeldNativeWritebackGate<'_>,
    preflight: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<bool>,
    operation: impl FnOnce(&Arc<CachedFileShared>, &FileNode) -> VfsResult<R>,
) -> VfsResult<Option<R>> {
    let shared = cached_file_shared_for_location_or_create(location);
    let end = offset
        .checked_add(u64::try_from(len).map_err(|_| VfsError::InvalidInput)?)
        .ok_or(VfsError::InvalidInput)?;
    let range_lease = CachedFileShared::try_range_cache_lease(&shared, offset..end, kind)?;
    let invalidation = {
        let _direct_guard = shared.direct_io_lock.lock();
        let _writeback_guard = shared.writeback_lock.write();
        wait_for_all_writeback_clear(&shared);
        let file = location.entry().as_file()?;
        let first_page = offset / PAGE_SIZE as u64;
        let last_page = end.div_ceil(PAGE_SIZE as u64);
        let first_page = u32::try_from(first_page).map_err(|_| VfsError::InvalidInput)?;
        let last_page = u32::try_from(last_page).map_err(|_| VfsError::InvalidInput)?;
        let mut invalidation = CachedPageInvalidationTransaction::new_shared(shared.clone());
        invalidation.stage_range(first_page..last_page)?;
        invalidation.prepare_evictions()?;
        invalidation.writeback_with_held_native_gate(file, true, native_gate)?;
        invalidation
    };
    let file = location.entry().as_file()?;
    if !preflight(&shared, file)? {
        // Drop without commit restores clean and dirty pages exactly as they
        // were observed before this unpublished direct attempt.
        return Ok(None);
    }
    debug_assert!(range_lease.revalidate());
    invalidation.commit_discard();
    let file = location.entry().as_file()?;
    operation(&shared, file).map(Some)
}

// Cache-invalidation machinery for the in-progress write path.
#[allow(dead_code)]
pub(super) fn with_cache_invalidating_truncate(location: &Location, len: u64) -> VfsResult<()> {
    let native_mutation = begin_native_location_mutation(location, false)?;
    with_cache_invalidating_truncate_with_held_native(
        location,
        len,
        held_native_writeback_gate(&native_mutation),
    )
}

pub(super) fn with_cache_invalidating_truncate_with_held_native(
    location: &Location,
    len: u64,
    native_gate: HeldNativeWritebackGate<'_>,
) -> VfsResult<()> {
    let shared = cached_file_shared_for_location_or_create(location);
    let _direct_guard = shared.direct_io_lock.lock();
    let mutation = CachedFile::begin_shared_cache_invalidating_mutation(&shared)?;
    let _writeback_guard = shared.writeback_lock.write();
    wait_for_all_writeback_clear(&shared);
    let file = location.entry().as_file()?;
    let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
    invalidation.stage_all()?;
    invalidation.prepare_evictions()?;
    invalidation.writeback_with_held_native_gate(file, false, native_gate)?;

    let failure_is_atomic = file.set_len_failure_is_atomic();
    if let Err(error) = file.set_len(len) {
        if !failure_is_atomic {
            // The lower filesystem may have published a partial truncate.
            // Retaining pre-operation cache pages would expose stale data.
            invalidation.commit_discard();
            release_cached_file_writeback_anchor_if_clean(&shared);
            release_closed_cached_file_retention(location);
        }
        return Err(error);
    }
    invalidation.commit_discard();
    release_cached_file_writeback_anchor_if_clean(&shared);
    release_closed_cached_file_retention(location);
    Ok(())
}

/// Runs one out-of-band content mutation between two cache invalidations.
///
/// Direct-I/O exclusion and pin-mutation admission remain held through the
/// lower operation, so no pin window can enter between invalidation and commit.
pub fn with_sync_and_invalidate_cached_file_pages<R>(
    location: &Location,
    operation: impl FnOnce() -> VfsResult<R>,
) -> VfsResult<R> {
    with_cache_invalidating_file_operation(location, |_, _| operation())
}

/// Flushes and drops cached pages before backend storage is changed out-of-band.
pub fn sync_and_invalidate_cached_file_pages(location: &Location) -> VfsResult<()> {
    with_sync_and_invalidate_cached_file_pages(location, || Ok(()))
}

pub(super) fn discard_cached_pages(shared: &Arc<CachedFileShared>) -> VfsResult<()> {
    let _direct_guard = shared.direct_io_lock.lock();
    let mutation = CachedFile::begin_shared_cache_invalidating_mutation(shared)?;
    let _writeback_guard = shared.writeback_lock.write();
    wait_for_all_writeback_clear(shared);
    let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
    invalidation.stage_all()?;
    invalidation.prepare_evictions()?;
    invalidation.commit_discard();
    Ok(())
}

/// Tries one task-context cleanup after unlinking has made the file eligible
/// for whole-file discard. The range-lease table is the synchronization
/// authority: a `ResourceBusy` result means another exact lease is still
/// live, so the request remains pending until that lease's Drop calls this
/// helper again. Other errors remain terminal and are not silently swallowed.
pub(super) fn attempt_unlinked_cached_file_cleanup(shared: &Arc<CachedFileShared>) -> bool {
    if !shared.unlinked.load(Ordering::Acquire)
        || shared.open_handles.load(Ordering::Acquire) != 0
        || shared.mount_roots.load(Ordering::Acquire) != 0
    {
        return false;
    }
    // Serialize the synchronous attempts themselves. A last range lease can
    // clear its table slot while an earlier attempt is still observing the
    // old conflict; keeping the request bit pending behind this guard lets
    // that lease-drop attempt run immediately after the earlier Busy result.
    let _cleanup_guard = shared.unlinked_cleanup_lock.lock();
    if shared.open_handles.load(Ordering::Acquire) != 0
        || shared.mount_roots.load(Ordering::Acquire) != 0
    {
        return false;
    }
    if shared
        .unlinked_cleanup_pending
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }

    match discard_cached_pages(shared) {
        Ok(()) => {
            shared
                .identity_lease
                .discarded_unlinked
                .store(true, Ordering::Release);
            release_cached_file_writeback_anchor_if_clean(shared);
            release_unlinked_cached_file_registry_ownership_for_shared(shared);
            true
        }
        Err(VfsError::ResourceBusy) => {
            // The exact lease owner will synchronously retry when it drops.
            shared
                .unlinked_cleanup_pending
                .store(true, Ordering::Release);
            false
        }
        Err(error) => {
            // Busy is the only expected transient outcome. Preserve the
            // existing fail-stop behavior for an actual cache/metadata error
            // rather than converting it into a silent cleanup success.
            shared
                .unlinked_cleanup_pending
                .store(true, Ordering::Release);
            panic!("failed to discard unlinked cached pages: {error:?}");
        }
    }
}

pub(super) fn request_unlinked_cached_file_cleanup(shared: &Arc<CachedFileShared>) -> bool {
    if !shared.unlinked.load(Ordering::Acquire)
        || shared.open_handles.load(Ordering::Acquire) != 0
        || shared.mount_roots.load(Ordering::Acquire) != 0
    {
        return false;
    }
    shared
        .unlinked_cleanup_pending
        .store(true, Ordering::Release);
    attempt_unlinked_cached_file_cleanup(shared)
}

pub(super) fn pop_unpinned_lru_page(
    cache: &mut LruCache<u32, PageCache>,
) -> VfsResult<Option<(u32, PageCache)>> {
    let mut skipped = 0;
    let limit = cache.len();
    while skipped < limit {
        let Some((pn, page)) = cache.peek_lru() else {
            return Ok(None);
        };
        let pn = *pn;
        if page.is_pinned() {
            cache.promote(&pn);
            skipped += 1;
            continue;
        }
        let popped = cache.pop_lru();
        if let Some((_, page)) = &popped {
            file_cache_remove_page(page);
        }
        return Ok(popped);
    }
    if limit == 0 {
        Ok(None)
    } else {
        Err(VfsError::ResourceBusy)
    }
}

#[derive(Default)]
pub(super) struct CleanPageScan {
    pub(super) page: Option<(u32, PageCache)>,
    pub(super) scanned: usize,
    pub(super) dirty: usize,
    pub(super) pinned: usize,
    pub(super) writeback: usize,
}

pub(super) fn pop_clean_unpinned_lru_page(
    cache: &mut LruCache<u32, PageCache>,
    scan_budget: usize,
) -> CleanPageScan {
    let mut scan = CleanPageScan::default();
    let mut fallback = None;
    let limit = cache.len().min(scan_budget);
    while scan.scanned < limit {
        let Some((pn, page)) = cache.peek_lru() else {
            break;
        };
        let pn = *pn;
        let dirty = page.is_dirty();
        let pinned = page.is_pinned();
        let writeback = page.is_writeback();
        let active = page.is_active();
        scan.scanned += 1;
        scan.dirty += usize::from(dirty);
        scan.pinned += usize::from(pinned);
        scan.writeback += usize::from(writeback);
        if active {
            cache.get_mut(&pn).unwrap().demote_active();
            file_cache_active_sub(1);
            cache.promote(&pn);
            continue;
        }
        if !dirty && !pinned && !writeback {
            if page.is_noreuse() {
                scan.page = cache.pop_lru();
                if let Some((_, page)) = &scan.page {
                    file_cache_remove_page(page);
                }
                return scan;
            }
            fallback.get_or_insert(pn);
        }
        // Rotate an ineligible LRU entry so a bounded scan can inspect every
        // resident page without allocating a side list.
        cache.promote(&pn);
    }
    if let Some(pn) = fallback {
        scan.page = cache.pop(&pn).map(|page| (pn, page));
        if let Some((_, page)) = &scan.page {
            file_cache_remove_page(page);
        }
    }
    scan
}

pub(super) fn pop_unused_readahead_lru_page(
    cache: &mut LruCache<u32, PageCache>,
) -> Option<(u32, PageCache)> {
    // NOREUSE pages are explicitly reclaim-priority candidates, not merely
    // a hint that happens to work when they reach the LRU head. Rotate each
    // noncandidate once so a bounded cache walk finds one anywhere in LRU.
    for _ in 0..cache.len() {
        let Some((pn, page)) = cache.peek_lru() else {
            return None;
        };
        let pn = *pn;
        if page.is_unused_prefetched() {
            let popped = cache.pop_lru();
            if let Some((_, page)) = &popped {
                file_cache_remove_page(page);
                record_readahead_retired_unused_page();
            }
            return popped;
        }
        cache.promote(&pn);
    }
    None
}

pub(super) fn restore_popped_cache_page(
    cache: &mut LruCache<u32, PageCache>,
    pn: u32,
    page: PageCache,
) {
    file_cache_restore_page(&page);
    assert!(
        cache.put(pn, page).is_none(),
        "restoring an evicted cache page replaced page {pn}"
    );
    cache.demote(&pn); // LRU recency only; preserves PageCache::active.
}

/// Moves selected keys to the LRU end while retaining their encounter order.
/// Calling `demote` in reverse order makes this a stable partition: selected
/// entries become the cold prefix, and every unselected entry retains its
/// relative order.
pub(super) fn stable_demote_lru_keys<T>(cache: &mut LruCache<u32, T>, keys_lru_to_mru: &[u32]) {
    for pn in keys_lru_to_mru.iter().rev() {
        cache.demote(pn);
    }
}

/// Collect page-cache keys in LRU order without turning advisory reclamation
/// into a mandatory allocation. `None` means pressure prevented the optional
/// resident reprioritization; callers must retain their future policy and
/// still report advisory success.
pub(super) fn try_collect_noreuse_keys<T>(
    cache: &LruCache<u32, T>,
    offset: u64,
    end: u64,
    reserve: usize,
) -> Option<Vec<u32>> {
    let mut keys = Vec::new();
    keys.try_reserve_exact(reserve).ok()?;
    for (pn, _) in cache.iter().rev() {
        let page_start = u64::from(*pn).saturating_mul(PAGE_SIZE as u64);
        if page_start >= offset && page_start < end {
            keys.push(*pn);
        }
    }
    Some(keys)
}

/// A file mount keeps its inode cache alive independently of open descriptors.
/// The lease owns no Location or Filesystem, so writeback anchors cannot keep
/// the mount alive through a cache-to-mount cycle.
pub struct CachedFileMountLease {
    pub(super) shared: Arc<CachedFileShared>,
}

impl CachedFileMountLease {
    pub fn new(location: &Location) -> VfsResult<Self> {
        let shared = cached_file_shared_for_location_or_create(location);
        {
            let _cleanup = shared.unlinked_cleanup_lock.lock();
            // The generation marker survives registry retirement. A bind
            // racing the final unlink/close must not resurrect discarded data.
            if shared
                .identity_lease
                .discarded_unlinked
                .load(Ordering::Acquire)
            {
                return Err(VfsError::NotFound);
            }
            shared.mount_roots.fetch_add(1, Ordering::AcqRel);
        }
        Ok(Self { shared })
    }
}

impl Drop for CachedFileMountLease {
    fn drop(&mut self) {
        if self.shared.mount_roots.fetch_sub(1, Ordering::AcqRel) == 1 {
            request_unlinked_cached_file_cleanup(&self.shared);
        }
    }
}

/// Marks cached pages for an inode whose final directory entry is being removed.
pub fn mark_cached_file_unlinked(location: &Location) {
    if let Some(shared) = cached_file_shared_for_location(location) {
        shared.unlinked.store(true, Ordering::Release);
        if shared.open_handles.load(Ordering::Acquire) == 0 {
            request_unlinked_cached_file_cleanup(&shared);
        }
        release_closed_cached_file_retention(location);
    }
}

/// Flushes all live dirty cached file pages before a global sync.
pub fn sync_all_cached_file_pages() -> VfsResult<()> {
    let entries = cached_file_writeback_snapshot();

    for (_key, shared, location) in entries {
        if shared.unlinked.load(Ordering::Acquire) {
            continue;
        }
        let dirty_pages = cached_dirty_page_numbers(&shared);
        if dirty_pages.is_empty() {
            continue;
        }
        let file = location.entry().as_file()?;
        flush_dirty_page_list(&shared, file, dirty_pages, false)?;
    }
    Ok(())
}

/// Flushes live dirty pages owned by one filesystem instance.
///
/// Filesystem backends call this before flushing their own metadata and block
/// caches so unmount and syncfs preserve writeback ordering.
pub fn sync_cached_file_pages_for_filesystem(filesystem: &dyn FilesystemOps) -> VfsResult<()> {
    let entries = cached_file_writeback_snapshot();

    for (_key, shared, location) in entries {
        if !core::ptr::addr_eq(location.filesystem(), filesystem)
            || shared.unlinked.load(Ordering::Acquire)
        {
            continue;
        }
        let dirty_pages = cached_dirty_page_numbers(&shared);
        if dirty_pages.is_empty() {
            continue;
        }
        let file = location.entry().as_file()?;
        flush_dirty_page_list(&shared, file, dirty_pages, false)?;
    }
    Ok(())
}
