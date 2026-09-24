//! Dirty-page writeback runs and cache flushing.

use super::*;

pub(super) fn begin_file_node_writeback_mutation(
    file: &FileNode,
) -> VfsResult<Option<FileAttrMutationGuard>> {
    if file.file_attr_provider().is_none() {
        return Ok(None);
    }
    // Same contract as begin_source_location_writeback_mutation: without
    // attached runtime user data no attr update can reach this inode, so
    // writeback proceeds without a gate rather than failing outright.
    let Some(data) = file.persistent_user_data() else {
        return Ok(None);
    };
    let guard = data.begin_file_attr_mutation();
    let attr = file.get_file_attr()?;
    // Writeback commits cached data to the provider. It is not a typed append
    // transaction, so both immutable and append state reject it at commit.
    if attr.xflags & (0x0000_0008 | 0x0000_0010) != 0 {
        return Err(VfsError::OperationNotPermitted);
    }
    Ok(Some(guard))
}

pub(super) fn writeback_cached_page_data(
    file: &FileNode,
    pn: u32,
    page: &mut PageCache,
) -> VfsResult<usize> {
    let native_mutation = begin_file_node_writeback_mutation(file)?;
    writeback_cached_page_data_with_held_native_gate(
        file,
        pn,
        page,
        held_native_writeback_gate(&native_mutation),
    )
}

/// Writes a staged dirty page.  A multi-inode reflink/dedupe transaction may
/// already own this inode's fileattr gate in ObjectKey order; in that case the
/// caller supplies the admission and this helper must not re-enter it.
pub(super) fn writeback_cached_page_data_with_held_native_gate(
    file: &FileNode,
    pn: u32,
    page: &mut PageCache,
    _native_gate: HeldNativeWritebackGate<'_>,
) -> VfsResult<usize> {
    if !page.dirty {
        return Ok(0);
    }
    let page_start = pn as u64 * PAGE_SIZE as u64;
    let len = (file.len()?.saturating_sub(page_start)).min(PAGE_SIZE as u64) as usize;
    if len == 0 {
        return Ok(0);
    }
    // Writeback is an actual provider mutation.  The typed gate makes the
    // admission lifetime explicit and prevents accidental recursive locking.
    let written = file.write_at(&page.data()[..len], page_start)?;
    crate::account_backing_write(written);
    match written {
        written if written == len => Ok(written),
        _ => Err(VfsError::Io),
    }
}

pub(super) struct DirtyWritebackPage {
    pub(super) pn: u32,
    pub(super) data: Vec<u8>,
}

pub(super) struct DirtyWritebackRun {
    pub(super) page_start: u64,
    pub(super) bytes: usize,
    pub(super) pages: Vec<DirtyWritebackPage>,
}

pub(super) enum DirtyWritebackCopy {
    Run(DirtyWritebackRun),
    Empty,
    Busy,
    Stale,
}

pub(super) struct DirtySgWritebackPage {
    pub(super) pn: u32,
    pub(super) ptr: *const u8,
    pub(super) len: usize,
}

pub(super) struct DirtySgWritebackRun {
    pub(super) page_start: u64,
    pub(super) bytes: usize,
    pub(super) pages: Vec<DirtySgWritebackPage>,
}

pub(super) enum DirtySgWritebackBegin {
    Run(DirtySgWritebackRun),
    Empty,
    Fallback,
    Busy,
}

pub(super) fn wait_for_page_writeback_clear(shared: &CachedFileShared, pn: u32) {
    while shared
        .page_cache
        .lock()
        .get(&pn)
        .is_some_and(PageCache::is_writeback)
    {
        spin_loop();
    }
}

pub(super) fn wait_for_dirty_pages_writeback_clear(shared: &CachedFileShared, pages: &[u32]) {
    while {
        let mut guard = shared.page_cache.lock();
        pages
            .iter()
            .any(|pn| guard.get(pn).is_some_and(PageCache::is_writeback))
    } {
        spin_loop();
    }
}

pub(super) fn wait_for_all_writeback_clear(shared: &CachedFileShared) {
    while shared
        .page_cache
        .lock()
        .iter()
        .any(|(_pn, page)| page.is_writeback())
    {
        spin_loop();
    }
}

pub(super) fn cached_dirty_page_numbers(shared: &CachedFileShared) -> Vec<u32> {
    let guard = shared.page_cache.lock();
    guard
        .iter()
        .filter_map(|(pn, page)| page.is_dirty().then_some(*pn))
        .collect()
}

pub(super) fn copy_dirty_writeback_run(
    _shared: &CachedFileShared,
    guard: &mut LruCache<u32, PageCache>,
    dirty_pages: &[u32],
    file_len: u64,
) -> VfsResult<DirtyWritebackCopy> {
    let Some(first_pn) = dirty_pages.first().copied() else {
        return Ok(DirtyWritebackCopy::Empty);
    };
    let page_start = first_pn as u64 * PAGE_SIZE as u64;
    let max_len = file_len
        .saturating_sub(page_start)
        .min((dirty_pages.len() * PAGE_SIZE) as u64) as usize;
    if max_len == 0 {
        for pn in dirty_pages {
            if let Some(page) = guard.get_mut(pn) {
                page.clear_dirty();
            }
        }
        return Ok(DirtyWritebackCopy::Empty);
    }

    // The dirty snapshot is advisory. Do not copy a clean/stale page into a
    // run whose byte offsets were derived from the original list; reselect it
    // after releasing any existing writeback instead.
    for pn in dirty_pages {
        match guard.get(pn) {
            Some(page) if page.is_writeback() => return Ok(DirtyWritebackCopy::Busy),
            Some(page) if page.is_dirty() => {}
            _ => return Ok(DirtyWritebackCopy::Stale),
        }
    }
    let mut pages: Vec<DirtyWritebackPage> = Vec::with_capacity(dirty_pages.len());
    for (idx, pn) in dirty_pages.iter().enumerate() {
        let Some(page) = guard.get_mut(pn) else {
            continue;
        };
        let dst_start = idx * PAGE_SIZE;
        if dst_start >= max_len {
            continue;
        }
        let len = (max_len - dst_start).min(PAGE_SIZE);
        let mut data = vec![0; len];
        data.copy_from_slice(&page.data()[..len]);
        pages.push(DirtyWritebackPage { pn: *pn, data });
    }

    Ok((!pages.is_empty())
        .then_some(DirtyWritebackRun {
            page_start,
            bytes: max_len,
            pages,
        })
        .map_or(DirtyWritebackCopy::Empty, DirtyWritebackCopy::Run))
}

pub(super) fn begin_sg_dirty_writeback_run(
    shared: &CachedFileShared,
    guard: &mut LruCache<u32, PageCache>,
    dirty_pages: &[u32],
    file_len: u64,
) -> VfsResult<DirtySgWritebackBegin> {
    let Some(first_pn) = dirty_pages.first().copied() else {
        return Ok(DirtySgWritebackBegin::Empty);
    };
    let page_start = first_pn as u64 * PAGE_SIZE as u64;
    let max_len = file_len
        .saturating_sub(page_start)
        .min((dirty_pages.len() * PAGE_SIZE) as u64) as usize;
    if max_len == 0 {
        for pn in dirty_pages {
            if let Some(page) = guard.get_mut(pn) {
                page.clear_dirty();
            }
        }
        return Ok(DirtySgWritebackBegin::Empty);
    }

    if dirty_pages.len() < 2 || max_len != dirty_pages.len() * PAGE_SIZE || max_len % PAGE_SIZE != 0
    {
        return Ok(DirtySgWritebackBegin::Fallback);
    }

    // File-backed mmap pages register evict listeners that may need to unmap
    // writable PTEs before writeback. The owned-buffer path has a snapshot
    // re-check after completion, so keep listener-backed pages on that path
    // until a full write-protect/generation protocol exists.
    if !shared.evict_listeners.lock().is_empty() {
        return Ok(DirtySgWritebackBegin::Fallback);
    }

    for pn in dirty_pages {
        let Some(page) = guard.get_mut(pn) else {
            return Ok(DirtySgWritebackBegin::Fallback);
        };
        if page.is_writeback() {
            return Ok(DirtySgWritebackBegin::Busy);
        }
        if !page.is_dirty() {
            return Ok(DirtySgWritebackBegin::Fallback);
        }
    }

    let mut pages: Vec<DirtySgWritebackPage> = Vec::with_capacity(dirty_pages.len());
    for pn in dirty_pages {
        let Some(page) = guard.get_mut(pn) else {
            for pinned in &pages {
                if let Some(page) = guard.get_mut(&pinned.pn) {
                    page.end_writeback();
                }
            }
            return Ok(DirtySgWritebackBegin::Fallback);
        };
        if let Err(err) = page.begin_writeback() {
            for pinned in &pages {
                if let Some(page) = guard.get_mut(&pinned.pn) {
                    page.end_writeback();
                }
            }
            return Err(err);
        }
        pages.push(DirtySgWritebackPage {
            pn: *pn,
            ptr: page.data().as_ptr(),
            len: PAGE_SIZE,
        });
    }

    Ok(DirtySgWritebackBegin::Run(DirtySgWritebackRun {
        page_start,
        bytes: max_len,
        pages,
    }))
}

pub(super) fn finish_sg_dirty_writeback_run(
    shared: &CachedFileShared,
    run: &DirtySgWritebackRun,
    success: bool,
) {
    let mut guard = shared.page_cache.lock();
    for written in &run.pages {
        let Some(page) = guard.get_mut(&written.pn) else {
            debug!(
                "missing page-cache page {} while ending SG writeback",
                written.pn
            );
            continue;
        };
        if success {
            page.clear_dirty();
        }
        page.end_writeback();
    }
}

pub(super) fn begin_dirty_writeback_run(
    shared: &CachedFileShared,
    run: &DirtyWritebackRun,
) -> VfsResult<()> {
    let mut guard = shared.page_cache.lock();
    let mut begun = Vec::new();
    for written in &run.pages {
        let Some(page) = guard.get_mut(&written.pn) else {
            for pn in begun {
                guard
                    .get_mut(&pn)
                    .expect("begun page disappeared")
                    .end_writeback();
            }
            return Err(VfsError::ResourceBusy);
        };
        if !page.is_dirty() || page.is_writeback() {
            for pn in begun {
                guard
                    .get_mut(&pn)
                    .expect("begun page disappeared")
                    .end_writeback();
            }
            return Err(VfsError::ResourceBusy);
        }
        if let Err(error) = page.begin_writeback() {
            for pn in begun {
                guard
                    .get_mut(&pn)
                    .expect("begun page disappeared")
                    .end_writeback();
            }
            return Err(error);
        }
        begun.push(written.pn);
    }
    Ok(())
}

/// Fence mapped writers while their snapshot is written back. The run's
/// writeback pins keep these exact frames resident while listener callbacks
/// acquire address spaces without holding the page-cache lock.
pub(super) fn prepare_dirty_writeback_aliases(
    shared: &CachedFileShared,
    run: &DirtyWritebackRun,
) -> VfsResult<CachedPageEvictionReservations> {
    let listeners = evict_listeners_snapshot(shared)?;
    let count = run
        .pages
        .len()
        .checked_mul(listeners.len())
        .ok_or(VfsError::NoMemory)?;
    let mut reservations = CachedPageEvictionReservations::reserve(count)?;
    if listeners.is_empty() {
        return Ok(reservations);
    }
    for written in &run.pages {
        let paddr = shared
            .page_cache
            .lock()
            .peek(&written.pn)
            .ok_or(VfsError::ResourceBusy)?
            .paddr();
        reservations.prepare(
            &listeners,
            CachedPageEviction {
                identity: shared.registry_key,
                page_number: written.pn,
                paddr,
                writeback_only: true,
            },
        )?;
    }
    Ok(reservations)
}

pub(super) fn finish_dirty_writeback_run(
    shared: &CachedFileShared,
    run: &DirtyWritebackRun,
    success: bool,
) {
    let mut guard = shared.page_cache.lock();
    for written in &run.pages {
        let Some(page) = guard.get_mut(&written.pn) else {
            continue;
        };
        if success
            && page.is_dirty()
            && page.data().get(..written.data.len()) == Some(written.data.as_slice())
        {
            page.clear_dirty();
        }
        page.end_writeback();
    }
}

pub(super) fn build_dirty_writeback_segments(run: &DirtyWritebackRun) -> Vec<Vec<u8>> {
    let target_len = DIRTY_WRITEBACK_SEGMENT_PAGES * PAGE_SIZE;
    let mut segments = Vec::with_capacity(run.pages.len().div_ceil(DIRTY_WRITEBACK_SEGMENT_PAGES));
    let mut current = Vec::new();
    for page in &run.pages {
        if !current.is_empty() && current.len() + page.data.len() > target_len {
            segments.push(current);
            current = Vec::new();
        }
        current.extend_from_slice(&page.data);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

pub(super) fn record_dirty_writeback(
    range_flush: bool,
    pages: usize,
    bytes: usize,
    async_enabled: bool,
) {
    if range_flush {
        record_cached_file_counter(&RANGE_FLUSH_DIRTY_PAGES, pages as u64);
        record_cached_file_counter(&RANGE_FLUSH_BYTES, bytes as u64);
    } else {
        record_cached_file_counter(&FLUSH_DIRTY_PAGES, pages as u64);
        record_cached_file_counter(&FLUSH_BYTES, bytes as u64);
    }
    if async_enabled {
        record_cached_file_counter(&ASYNC_DIRTY_FLUSH_HITS, 1);
        record_cached_file_counter(&ASYNC_DIRTY_FLUSH_PAGES, pages as u64);
        record_cached_file_counter(&ASYNC_DIRTY_FLUSH_BYTES, bytes as u64);
    }
}

pub(super) fn record_async_dirty_flush_sg(pages: usize) {
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_SG_HITS, 1);
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_SG_SEGMENTS, pages as u64);
}

pub(super) fn record_async_dirty_flush_sg_async_submit(pages: usize) {
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_HITS, 1);
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_SG_ASYNC_SUBMIT_SEGMENTS, pages as u64);
}

pub(super) fn record_async_dirty_flush_bounce_fallback() {
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_BOUNCE_FALLBACKS, 1);
}

pub(super) fn record_async_dirty_flush_writeback_restart() {
    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_WRITEBACK_RESTARTS, 1);
}

/// Records a completed asynchronous dirty-page writeback failure on the
/// backend inode that owns the cached pages.  Do not use this for synchronous
/// writeback or the subsequent `fsync` metadata flush: those are returned to
/// their immediate caller instead of becoming an errseq event.
pub(super) fn publish_async_dirty_writeback_completion_error(file: &FileNode, error: VfsError) {
    if let Ok(state) = file.writeback_error_state() {
        state.publish(error);
    }
    // An accepted asynchronous completion also belongs to the filesystem's
    // superblock errseq.  Synchronous writeback and explicit fsync failures
    // never pass through this completion-only hook.
    if let Some(state) = file.syncfs_writeback_error_state() {
        state.publish(error);
    }
}

pub(super) fn async_dirty_flush_sg_enabled() -> bool {
    ENABLE_ASYNC_DIRTY_FLUSH_SG.load(Ordering::Relaxed)
}

pub(super) fn cached_readahead_enabled() -> bool {
    ENABLE_CACHED_READAHEAD.load(Ordering::Relaxed)
}

pub(super) fn record_readahead_miss() {
    record_cached_file_counter(&READAHEAD_MISSES, 1);
}

pub(super) fn record_readahead_window(pages: usize) {
    if pages == 0 {
        return;
    }
    record_cached_file_counter(&READAHEAD_WINDOWS, 1);
    record_cached_file_counter(&READAHEAD_PAGES_LOADED, pages as u64);
}

pub(super) fn record_readahead_hit() {
    record_cached_file_counter(&READAHEAD_HITS, 1);
}

pub(super) fn record_readahead_retired_unused_page() {
    record_cached_file_counter(&READAHEAD_RETIRED_UNUSED_PAGES, 1);
}

pub(super) fn record_file_sync_request(data_only: bool) {
    if data_only {
        record_cached_file_counter(&SYNC_DATA_ONLY_REQUESTS, 1);
    } else {
        record_cached_file_counter(&SYNC_METADATA_REQUESTS, 1);
    }
}

pub(crate) fn record_file_sync_data_only_metadata_fallback() {
    record_cached_file_counter(&SYNC_DATA_ONLY_METADATA_FALLBACKS, 1);
}

pub(super) struct DirtyWritebackError {
    pub(super) error: VfsError,
    pub(super) errseq_published: bool,
    pub(super) worker_must_publish: bool,
}

impl From<VfsError> for DirtyWritebackError {
    fn from(error: VfsError) -> Self {
        Self {
            error,
            errseq_published: false,
            worker_must_publish: false,
        }
    }
}

impl DirtyWritebackError {
    /// Converts the lower-level result returned by a real page writeback.
    /// This is the only error class a queued range-writeback worker may turn
    /// into an asynchronous errseq event.  SG completion failures are already
    /// published by the submit/completion path; the synchronous fallback is
    /// deliberately deferred until the range worker owns its completion.
    pub(super) fn completion(error: VfsError, errseq_published: bool) -> Self {
        Self {
            error,
            errseq_published,
            worker_must_publish: true,
        }
    }
}

/// A range-writeback error that has reached the page writeback layer.  Setup
/// failures (range leases, node lookup, and argument validation) remain plain
/// VFS errors and must not be published as asynchronous completion failures.
pub(super) enum RangeSyncError {
    Immediate(VfsError),
    Writeback(DirtyWritebackError),
}

pub(super) fn flush_dirty_page_list_locked_with_held_native_gate(
    shared: &CachedFileShared,
    file: &FileNode,
    mut dirty_pages: Vec<u32>,
    range_flush: bool,
    _native_gate: HeldNativeWritebackGate<'_>,
) -> Result<(), DirtyWritebackError> {
    let file_len = file.len()?;
    dirty_pages.sort_unstable();

    let mut start = 0;
    while start < dirty_pages.len() {
        let async_enabled = virtio_async_block_enabled();
        let dirty_run_limit = if async_enabled
            && async_dirty_flush_sg_enabled()
            && virtio_async_block_wait_policy() == AsyncBlockWaitPolicy::InterruptFirst
        {
            IRQ_FIRST_DIRTY_WRITEBACK_PAGES
        } else {
            MAX_DIRTY_WRITEBACK_PAGES
        };
        let end_limit = (start + dirty_run_limit).min(dirty_pages.len());
        let mut end = start + 1;
        while end < end_limit && dirty_pages[end] == dirty_pages[end - 1] + 1 {
            end += 1;
        }

        if async_enabled && async_dirty_flush_sg_enabled() {
            let sg_begin = {
                let mut guard = shared.page_cache.lock();
                begin_sg_dirty_writeback_run(
                    shared,
                    &mut guard,
                    &dirty_pages[start..end],
                    file_len,
                )?
            };
            match sg_begin {
                DirtySgWritebackBegin::Run(run) => {
                    let slices = run
                        .pages
                        .iter()
                        .map(|page| unsafe { core::slice::from_raw_parts(page.ptr, page.len) })
                        .collect::<Vec<_>>();
                    let mut accepted_async_submit = false;
                    let write_result =
                        match file.try_write_at_vectored_async(&slices, run.page_start) {
                            Ok(AsyncVectoredWriteOutcome::Completed(written)) => {
                                accepted_async_submit = true;
                                Ok(written)
                            }
                            Ok(AsyncVectoredWriteOutcome::CompletionError(error)) => {
                                accepted_async_submit = true;
                                Err(error)
                            }
                            Ok(AsyncVectoredWriteOutcome::NotSubmitted) => {
                                file.write_at_vectored(&slices, run.page_start)
                            }
                            Err(error) => Err(error),
                        };
                    if let Ok(written) = write_result.as_ref() {
                        crate::account_backing_write(*written);
                    }
                    match write_result {
                        Ok(written) if written == run.bytes => {
                            record_dirty_writeback(range_flush, run.pages.len(), run.bytes, true);
                            record_async_dirty_flush_sg(run.pages.len());
                            if accepted_async_submit {
                                record_async_dirty_flush_sg_async_submit(run.pages.len());
                            }
                            finish_sg_dirty_writeback_run(shared, &run, true);
                        }
                        Ok(_) => {
                            record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                            if accepted_async_submit {
                                publish_async_dirty_writeback_completion_error(file, VfsError::Io);
                            }
                            finish_sg_dirty_writeback_run(shared, &run, false);
                            return Err(DirtyWritebackError {
                                error: VfsError::Io,
                                errseq_published: accepted_async_submit,
                                worker_must_publish: false,
                            });
                        }
                        Err(err) => {
                            record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                            if accepted_async_submit {
                                publish_async_dirty_writeback_completion_error(file, err);
                            }
                            finish_sg_dirty_writeback_run(shared, &run, false);
                            return Err(DirtyWritebackError {
                                error: err,
                                errseq_published: accepted_async_submit,
                                worker_must_publish: false,
                            });
                        }
                    }
                    start = end;
                    continue;
                }
                DirtySgWritebackBegin::Empty => {
                    start = end;
                    continue;
                }
                DirtySgWritebackBegin::Busy => {
                    record_async_dirty_flush_writeback_restart();
                    wait_for_dirty_pages_writeback_clear(shared, &dirty_pages[start..end]);
                    dirty_pages = cached_dirty_page_numbers(shared);
                    start = 0;
                    continue;
                }
                DirtySgWritebackBegin::Fallback => {
                    record_async_dirty_flush_bounce_fallback();
                }
            }
        }

        let copy = {
            let mut guard = shared.page_cache.lock();
            copy_dirty_writeback_run(shared, &mut guard, &dirty_pages[start..end], file_len)
        }?;
        let run = match copy {
            DirtyWritebackCopy::Run(run) => run,
            DirtyWritebackCopy::Empty => {
                start = end;
                continue;
            }
            DirtyWritebackCopy::Busy => {
                record_async_dirty_flush_writeback_restart();
                wait_for_dirty_pages_writeback_clear(shared, &dirty_pages[start..end]);
                dirty_pages = cached_dirty_page_numbers(shared);
                start = 0;
                continue;
            }
            DirtyWritebackCopy::Stale => {
                dirty_pages = cached_dirty_page_numbers(shared);
                start = 0;
                continue;
            }
        };

        if let Err(error) = begin_dirty_writeback_run(shared, &run) {
            if error == VfsError::ResourceBusy {
                record_async_dirty_flush_writeback_restart();
                wait_for_dirty_pages_writeback_clear(shared, &dirty_pages[start..end]);
                dirty_pages = cached_dirty_page_numbers(shared);
                start = 0;
                continue;
            }
            return Err(error.into());
        }

        let aliases = match prepare_dirty_writeback_aliases(shared, &run) {
            Ok(aliases) => aliases,
            Err(error) => {
                finish_dirty_writeback_run(shared, &run, false);
                return Err(error.into());
            }
        };
        let segments = build_dirty_writeback_segments(&run);
        let slices = segments.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let write_result = file.write_at_vectored(&slices, run.page_start);
        if let Ok(written) = write_result.as_ref() {
            crate::account_backing_write(*written);
        }
        match write_result {
            Ok(written) if written == run.bytes => {
                record_dirty_writeback(range_flush, run.pages.len(), run.bytes, async_enabled);
                finish_dirty_writeback_run(shared, &run, true);
                // The next mapped write must fault and dirty the page again,
                // including when it occurs after this fsync has returned.
                aliases.commit();
            }
            Ok(_) => {
                finish_dirty_writeback_run(shared, &run, false);
                if async_enabled {
                    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                }
                return Err(DirtyWritebackError::completion(VfsError::Io, false));
            }
            Err(err) => {
                finish_dirty_writeback_run(shared, &run, false);
                if async_enabled {
                    record_cached_file_counter(&ASYNC_DIRTY_FLUSH_ERRORS, 1);
                }
                return Err(DirtyWritebackError::completion(err, false));
            }
        }
        start = end;
    }
    release_cached_file_writeback_anchor_if_clean(shared);
    Ok(())
}

pub(super) fn flush_dirty_page_list(
    shared: &Arc<CachedFileShared>,
    file: &FileNode,
    dirty_pages: Vec<u32>,
    range_flush: bool,
) -> VfsResult<()> {
    let _range_lease = CachedFileShared::try_range_cache_lease(
        shared,
        0..u64::MAX,
        RangeCacheLeaseKind::CachedWrite,
    )?;
    let _writeback_guard = shared.writeback_lock.read();
    let native_mutation = begin_file_node_writeback_mutation(file)?;
    flush_dirty_page_list_locked_with_held_native_gate(
        shared,
        file,
        dirty_pages,
        range_flush,
        held_native_writeback_gate(&native_mutation),
    )
    .map_err(|error| error.error)
}

pub(super) fn flush_dirty_cache_shared_locked_with_held_native_gate(
    shared: &CachedFileShared,
    file: &FileNode,
    native_gate: HeldNativeWritebackGate<'_>,
) -> VfsResult<()> {
    let dirty_pages = {
        let guard = shared.page_cache.lock();
        guard
            .iter()
            .filter_map(|(pn, page)| page.is_dirty().then_some(*pn))
            .collect::<Vec<_>>()
    };
    flush_dirty_page_list_locked_with_held_native_gate(
        shared,
        file,
        dirty_pages,
        false,
        native_gate,
    )
    .map_err(|error| error.error)
}

pub(super) fn flush_dirty_cache_shared(
    shared: &Arc<CachedFileShared>,
    file: &FileNode,
) -> VfsResult<()> {
    let _range_lease = CachedFileShared::try_range_cache_lease(
        shared,
        0..u64::MAX,
        RangeCacheLeaseKind::CachedWrite,
    )?;
    let _writeback_guard = shared.writeback_lock.read();
    let native_mutation = begin_file_node_writeback_mutation(file)?;
    flush_dirty_cache_shared_locked_with_held_native_gate(
        shared,
        file,
        held_native_writeback_gate(&native_mutation),
    )
}

pub(super) fn flush_dirty_cache_shared_with_held_native_gate(
    shared: &Arc<CachedFileShared>,
    file: &FileNode,
    native_gate: HeldNativeWritebackGate<'_>,
) -> VfsResult<()> {
    let _range_lease = CachedFileShared::try_range_cache_lease(
        shared,
        0..u64::MAX,
        RangeCacheLeaseKind::CachedWrite,
    )?;
    let _writeback_guard = shared.writeback_lock.read();
    flush_dirty_cache_shared_locked_with_held_native_gate(shared, file, native_gate)
}
