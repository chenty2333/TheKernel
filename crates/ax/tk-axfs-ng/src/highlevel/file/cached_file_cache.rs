//! CachedFile: eviction, fadvise, cache admission and range writeback.

use super::*;

impl CachedFile {
    /// Returns whether every page required by a NOWAIT operation is already
    /// resident and not under writeback.  This performs no allocation, lower
    /// I/O, cache fill, or wait.
    pub fn nowait_range_resident(&self, offset: u64, length: usize) -> bool {
        if length == 0 {
            return true;
        }
        let Some(end) = offset.checked_add(length as u64) else {
            return false;
        };
        let first = offset / PAGE_SIZE as u64;
        let last = end.saturating_sub(1) / PAGE_SIZE as u64;
        let Ok(mut first) = u32::try_from(first) else {
            return false;
        };
        let Ok(last) = u32::try_from(last) else {
            return false;
        };
        // Residency admission is part of the NOWAIT transaction. A contended
        // cache lock is not a reason to wait merely to discover a miss.
        let Some(mut cache) = self.shared.page_cache.try_lock() else {
            return false;
        };
        loop {
            let Some(page) = cache.get(&first) else {
                return false;
            };
            if page.is_writeback() {
                return false;
            }
            if first == last {
                return true;
            }
            let Some(next) = first.checked_add(1) else {
                return false;
            };
            first = next;
        }
    }
    /// Snapshot the completion generation used by an address-space eviction
    /// retry.  This is deliberately independent of page-cache residency: an
    /// abort and a commit are both valid retry edges.
    pub fn eviction_completion_epoch(&self) -> u64 {
        self.shared
            .eviction_completion_epoch
            .load(Ordering::Acquire)
    }

    /// Wait until a commit or abort has completed after `observed_epoch`.
    /// `WaitQueue::wait_until` performs check-arm-check, so completion between
    /// the caller's initial observation and listener registration is not lost.
    pub fn wait_for_eviction_completion(
        &self,
        observed_epoch: u64,
    ) -> Result<(), axtask::WaitError> {
        self.shared.eviction_completion.wait_until(|| {
            self.shared
                .eviction_completion_epoch
                .load(Ordering::Acquire)
                != observed_epoch
        })
    }

    /// Publish the terminal edge of one prepared alias reservation.  Kept
    /// public for the kernel listener implementation; it is not a userspace
    /// cache API.
    pub fn notify_eviction_completion(&self) {
        self.shared
            .eviction_completion_epoch
            .fetch_add(1, Ordering::Release);
        self.shared.eviction_completion.notify_all(false);
    }

    /// Snapshot the cache state in the inclusive page interval used by
    /// Linux's cachestat(2).  The snapshot is intentionally advisory: page
    /// state may change immediately after the locks are released.
    pub fn cachestat(&self, first_page: u64, last_page: u64) -> CachedFileCacheStat {
        self.shared.cachestat(first_page, last_page)
    }

    pub(super) fn record_eviction(&self, page_no: u32) {
        record_file_cache_shadow(&self.shared, page_no);
    }
    /// Returns an existing cached file for `location`, or creates a new one.
    pub fn get_or_create(location: Location) -> Self {
        let in_memory = cached_file_is_in_memory(&location);
        let shared = cached_file_shared_for_location_or_create(&location);
        shared.open_handles.fetch_add(1, Ordering::AcqRel);

        Self {
            inner: location,
            shared,
            in_memory,
        }
    }

    /// Returns a cache handle only when this inode already owns cached state.
    /// Unlike `get_or_create`, this performs no registry, identity, or Arc
    /// allocation and is therefore safe for best-effort advisory paths.
    pub(super) fn get_existing(location: Location) -> Option<Self> {
        let shared = cached_file_shared_for_location(&location)?;
        shared.open_handles.fetch_add(1, Ordering::AcqRel);
        Some(Self {
            in_memory: cached_file_is_in_memory(&location),
            inner: location,
            shared,
        })
    }

    /// Returns `true` if both handles refer to the same shared state.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// Returns the stable identity shared by the page cache and external
    /// users (for example shared-futex wait queues).
    pub fn identity(&self) -> CachedFileIdentity {
        self.shared.registry_key
    }

    /// Faults a bounded advised range into the coherent page cache. Page
    /// allocation and lower I/O happen before taking the cache lock; the
    /// range lease serializes this two-phase publication with DONTNEED.
    pub(super) fn fadvise_willneed_now(&self, offset: u64, len: u64) -> VfsResult<()> {
        let file = self.inner.entry().as_file()?;
        let max_len = FADVISE_WILLNEED_MAX_PAGES.saturating_mul(PAGE_SIZE as u64);
        let end = offset.saturating_add(len.min(max_len)).min(file.len()?);
        if end <= offset {
            return Ok(());
        }
        let first = offset / PAGE_SIZE as u64;
        let last = end.saturating_sub(1) / PAGE_SIZE as u64;
        for page in first..=last {
            let pn = u32::try_from(page).map_err(|_| VfsError::InvalidInput)?;
            let lease = CachedFileShared::try_range_cache_lease(
                &self.shared,
                page_range(page, 1),
                RangeCacheLeaseKind::CachedRead,
            )?;
            if self.shared.page_cache.lock().contains(&pn) {
                continue;
            }
            let mut prepared = PageCache::new(self.shared.in_memory)?;
            prepared.data().fill(0);
            let read = file.read_at(prepared.data(), page * PAGE_SIZE as u64)?;
            if !self.shared.in_memory {
                crate::account_backing_read(read);
            }
            // The lease stayed live through the lock-free read. Rechecking
            // its slot makes publication conditional on that exact lease.
            if !lease.revalidate() {
                continue;
            }
            let mut cache = self.shared.page_cache.lock();
            if cache.contains(&pn) || cache.len() == cache.cap().get() {
                continue;
            }
            prepared.mark_prefetched();
            // WILLNEED is another cache-publication path, not an exception
            // to workingset accounting.  Consume a reclaim shadow before
            // exposing the page so an immediately reused prefetched page has
            // the same refault semantics as synchronous and async fills.
            file_cache_apply_refault(&self.shared, pn, &mut prepared);
            // The capacity/duplicate checks above and this cache lock make a
            // replacement impossible.  Keep the accounting publication on
            // the successful insertion edge nevertheless: WILLNEED is a
            // normal resident-cache producer, and a failed publication must
            // not leave a resident count or active/refault state behind.
            if cache.put(pn, prepared).is_none() {
                file_cache_resident_add(1);
            } else {
                unreachable!("WILLNEED insertion replaced an existing cache page");
            }
            cache.demote(&pn);
        }
        Ok(())
    }

    /// Queue bounded best-effort prefetch.  The worker owns a `CachedFile`
    /// clone, so close/unlink cannot leave a dangling cache reference.
    pub fn fadvise_willneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        // This is a syscall path: construct its task name fallibly before
        // publishing a request, rather than relying on String's infallible
        // growth after a successful syscall return.
        let mut name = alloc::string::String::new();
        if name.try_reserve_exact("fadvise-ra".len()).is_err() {
            return Ok(());
        }
        name.push_str("fadvise-ra");
        let request = FadviseReadaheadRequest { offset, len };
        let generation = {
            let mut q = self.shared.fadvise_readahead.queue.lock();
            let _ = !q.contains(request) && q.push(request);
            if q.worker_running {
                return Ok(());
            }
            // Publish the generation before task construction. A worker can
            // therefore never finish and clear a bit that this caller writes
            // after spawning it; failure clears only this exact generation.
            q.worker_generation = q.worker_generation.wrapping_add(1);
            q.worker_running = true;
            q.worker_generation
        };
        let worker = self.clone();
        if axtask::try_spawn_with_name(move || worker.fadvise_readahead_worker(generation), name)
            .is_err()
        {
            let mut q = self.shared.fadvise_readahead.queue.lock();
            if q.worker_running && q.worker_generation == generation {
                q.worker_running = false;
            }
            // WILLNEED is explicitly best-effort: retain/defer queued work
            // for a later advisory call without exposing scheduler ENOMEM.
        }
        Ok(())
    }

    pub(super) fn fadvise_readahead_worker(&self, generation: u64) {
        loop {
            let request = {
                let mut q = self.shared.fadvise_readahead.queue.lock();
                if !q.worker_running || q.worker_generation != generation {
                    return;
                }
                match q.pop() {
                    Some(r) => r,
                    None => {
                        if q.worker_generation == generation {
                            q.worker_running = false;
                        }
                        return;
                    }
                }
            };
            let _ = self.fadvise_willneed_now(request.offset, request.len);
        }
    }

    /// Marks already resident clean pages as low-reuse candidates.  It never
    /// faults data in, which is the important distinction from WILLNEED.
    pub fn fadvise_noreuse(&self, offset: u64, len: u64) -> VfsResult<()> {
        let end = offset.saturating_add(len);
        if end <= offset {
            return Ok(());
        }
        let mut cache = self.shared.page_cache.lock();
        // Like DONTNEED, NOREUSE is range-local but must be O(resident), not
        // O(advised pages), for sparse or deliberately huge ranges. Gather
        // resident matches in LRU order with fallible storage, mark them
        // without promoting them, then stable-splice them to the cold end.
        let reserve = cache.len();
        let Some(keys) = try_collect_noreuse_keys(&cache, offset, end, reserve) else {
            // This path is a cache-only optimization. The OFD policy remains
            // active, so a later read still marks its consumed pages NOREUSE.
            return Ok(());
        };
        for pn in &keys {
            if let Some(entry) = cache.peek_mut(pn) {
                entry.mark_noreuse();
            }
        }
        stable_demote_lru_keys(&mut cache, &keys);
        Ok(())
    }

    /// Writes back and invalidates only whole pages fully covered by the
    /// range.  On a writeback or eviction failure the transaction drops and
    /// restores every staged page, retaining dirty data and its error state.
    pub fn fadvise_dontneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        let file = self.inner.entry().as_file()?;
        let native_mutation = begin_source_location_writeback_mutation(&self.inner)?;
        let file_len = file.len()?;
        let end = offset.saturating_add(len).min(file_len);
        let first = offset.saturating_add(PAGE_SIZE as u64 - 1) / PAGE_SIZE as u64;
        // A partial page in the middle of a file can contain bytes outside
        // the advised range.  The final EOF page has no such live suffix and
        // Linux may invalidate it as part of the through-EOF form.
        let last_exclusive = if end == file_len {
            end.div_ceil(PAGE_SIZE as u64)
        } else {
            end / PAGE_SIZE as u64
        };
        if first >= last_exclusive {
            return Ok(());
        }
        let pages = u32::try_from(first).map_err(|_| VfsError::InvalidInput)?
            ..u32::try_from(last_exclusive).map_err(|_| VfsError::InvalidInput)?;
        // This is a range-local hint, not an inode-wide direct-I/O
        // transition.  The lease excludes only aliases of these pages; pins
        // and eviction listeners are checked by the staged transaction.
        let byte_end = last_exclusive.saturating_mul(PAGE_SIZE as u64);
        let _lease = CachedFileShared::try_range_cache_lease(
            &self.shared,
            first.saturating_mul(PAGE_SIZE as u64)..byte_end,
            RangeCacheLeaseKind::DirectWrite,
        )?;
        let _writeback_guard = self.shared.writeback_lock.write();
        let mut invalidation = CachedPageInvalidationTransaction::new_shared(self.shared.clone());
        invalidation.stage_range(pages)?;
        invalidation.prepare_evictions()?;
        invalidation.writeback_with_held_native_gate(
            file,
            true,
            held_native_writeback_gate(&native_mutation),
        )?;
        invalidation.commit_discard();
        Ok(())
    }

    /// Opens a short preparation window for pinning file-backed user I/O pages.
    ///
    /// While this window is active, direct cache-draining I/O and LRU evictions
    /// are conservatively rejected for this cached file. Precise page pins take
    /// over once the caller has identified the exact cached pages.
    pub fn begin_user_io_pin_window(&self) -> VfsResult<CachedFilePinWindow> {
        let range_lease = Some(CachedFileShared::try_range_cache_lease(
            &self.shared,
            0..u64::MAX,
            RangeCacheLeaseKind::CachedWrite,
        )?);
        let mut admission = self.shared.user_io_pin_admission.lock();
        if admission.invalidating || admission.cache_users != 0 {
            return Err(VfsError::ResourceBusy);
        }
        admission.pin_windows = admission
            .pin_windows
            .checked_add(1)
            .ok_or(VfsError::NoMemory)?;
        drop(admission);
        Ok(CachedFilePinWindow {
            cache: self.clone(),
            _range_lease: range_lease,
        })
    }

    /// Pins an already cached page if it still maps to `paddr`.
    pub fn pin_cached_page_by_paddr(
        &self,
        pn: u32,
        paddr: PhysAddr,
        dirty_on_release: bool,
    ) -> VfsResult<CachedFilePagePin> {
        let admission = self.shared.user_io_pin_admission.lock();
        if admission.invalidating || admission.cache_users != 0 {
            return Err(VfsError::ResourceBusy);
        }
        let Some(mut guard) = self.shared.page_cache.try_lock() else {
            return Err(VfsError::ResourceBusy);
        };
        let Some(page) = guard.get_mut(&pn) else {
            return Err(VfsError::BadAddress);
        };
        if page.paddr() != paddr {
            return Err(VfsError::BadAddress);
        }
        let range_lease = Some(CachedFileShared::try_range_cache_lease(
            &self.shared,
            page_range(u64::from(pn), 1),
            if dirty_on_release {
                RangeCacheLeaseKind::CachedWrite
            } else {
                RangeCacheLeaseKind::CachedRead
            },
        )?);
        page.pin()?;
        Ok(CachedFilePagePin {
            cache: self.clone(),
            pn,
            dirty_on_release,
            _range_lease: range_lease,
        })
    }

    pub(super) fn begin_cache_invalidating_mutation(&self) -> VfsResult<CachedFileMutationGuard> {
        Self::begin_shared_cache_invalidating_mutation(&self.shared)
    }

    pub(super) fn begin_cache_user(&self) -> VfsResult<CachedFileCacheUserGuard> {
        self.begin_cache_user_range(0..u64::MAX, RangeCacheLeaseKind::CachedRead)
    }

    pub(super) fn begin_cache_user_range(
        &self,
        range: Range<u64>,
        kind: RangeCacheLeaseKind,
    ) -> VfsResult<CachedFileCacheUserGuard> {
        let range_lease = Some(CachedFileShared::try_range_cache_lease(
            &self.shared,
            range,
            kind,
        )?);
        let mut admission = self.shared.user_io_pin_admission.lock();
        if admission.invalidating || admission.pin_windows != 0 {
            return Err(VfsError::ResourceBusy);
        }
        admission.cache_users = admission
            .cache_users
            .checked_add(1)
            .ok_or(VfsError::NoMemory)?;
        drop(admission);
        Ok(CachedFileCacheUserGuard {
            shared: self.shared.clone(),
            _range_lease: range_lease,
        })
    }

    pub(super) fn begin_shared_cache_invalidating_mutation(
        shared: &Arc<CachedFileShared>,
    ) -> VfsResult<CachedFileMutationGuard> {
        Self::begin_shared_cache_invalidating_range(shared, 0..u64::MAX)
    }

    pub(super) fn begin_shared_cache_invalidating_range(
        shared: &Arc<CachedFileShared>,
        range: Range<u64>,
    ) -> VfsResult<CachedFileMutationGuard> {
        let range_lease = Some(CachedFileShared::try_range_cache_lease(
            shared,
            range,
            RangeCacheLeaseKind::WholeFileMutation,
        )?);
        let mut admission = shared.user_io_pin_admission.lock();
        if admission.invalidating || admission.cache_users != 0 || admission.pin_windows != 0 {
            return Err(VfsError::ResourceBusy);
        }
        admission.invalidating = true;
        drop(admission);
        Ok(CachedFileMutationGuard {
            shared: shared.clone(),
            _range_lease: range_lease,
        })
    }

    /// Admit the pressure scan while its caller holds `direct_io_lock`.
    /// Precise pins protect their individual pages, which the scan skips.
    /// A whole-file range lease would unnecessarily prevent reclaiming every
    /// other page for the entire lifetime of one registered user buffer.
    pub(super) fn try_begin_shared_cache_reclaim(
        shared: &Arc<CachedFileShared>,
    ) -> VfsResult<CachedFileMutationGuard> {
        let Some(mut admission) = shared.user_io_pin_admission.try_lock() else {
            return Err(VfsError::ResourceBusy);
        };
        if admission.invalidating || admission.cache_users != 0 || admission.pin_windows != 0 {
            return Err(VfsError::ResourceBusy);
        }
        admission.invalidating = true;
        drop(admission);
        Ok(CachedFileMutationGuard {
            shared: shared.clone(),
            _range_lease: None,
        })
    }

    pub(super) fn admit_truncate(&self, old_len: u64, new_len: u64) -> VfsResult<()> {
        if new_len >= old_len {
            return Ok(());
        }
        let guard = self.shared.page_cache.lock();
        let overlaps_pinned_page = guard.iter().any(|(pn, page)| {
            let page_end = u64::from(*pn)
                .saturating_add(1)
                .saturating_mul(PAGE_SIZE as u64);
            page_end > new_len && page.is_pinned()
        });
        if overlaps_pinned_page {
            Err(VfsError::ResourceBusy)
        } else {
            Ok(())
        }
    }

    /// Returns `true` if this file is backed by an in-memory filesystem (e.g. tmpfs).
    pub fn in_memory(&self) -> bool {
        self.in_memory
    }

    /// Registers a listener that is called when a page is evicted from cache.
    ///
    /// Returns a handle that can later be passed to
    /// [`remove_evict_listener`](Self::remove_evict_listener).
    pub fn add_evict_listener<F>(
        &self,
        owner: CachedFileEvictionOwner,
        listener: F,
    ) -> VfsResult<usize>
    where
        F: Fn(CachedPageEviction) -> VfsResult<Box<dyn CachedPageEvictionReservation>>
            + Send
            + Sync
            + 'static,
    {
        // Mutation admission precedes the listener table everywhere.  This
        // closes the registration/snapshot race: an eviction either sees this
        // listener or registration observes the active mutation and retries.
        let admission = self.shared.user_io_pin_admission.lock();
        if admission.invalidating {
            return Err(VfsError::ResourceBusy);
        }
        let pointer = Box::new(EvictListener {
            owner,
            listener: Arc::new(listener),
            link: LinkedListAtomicLink::new(),
        });
        let handle = pointer.as_ref() as *const EvictListener as usize;
        self.shared.evict_listeners.lock().push_back(pointer);
        drop(admission);
        Ok(handle)
    }

    /// # Safety
    /// The handle must be valid, that means:
    /// - It must be returned by a previous call to `add_evict_listener` on the same `CachedFile`.
    /// - It must not be removed by a previous call to `remove_evict_listener`.
    pub unsafe fn remove_evict_listener(&self, handle: usize) {
        let mut guard = self.shared.evict_listeners.lock();
        let mut cursor = unsafe { guard.cursor_mut_from_ptr(handle as *const EvictListener) };
        cursor.remove();
    }

    pub(super) fn evict_cache(
        &self,
        file: &FileNode,
        listeners: &[EvictListenerSnapshot],
        pn: u32,
        page: &mut PageCache,
    ) -> VfsResult<()> {
        if page.is_pinned() {
            return Err(VfsError::ResourceBusy);
        }
        let mut reservations = CachedPageEvictionReservations::reserve(listeners.len())?;
        reservations.prepare(
            listeners,
            CachedPageEviction {
                identity: self.identity(),
                page_number: pn,
                paddr: page.paddr(),
                writeback_only: false,
            },
        )?;
        let _ = writeback_cached_page_data(file, pn, page)?;
        reservations.commit();
        page.clear_dirty();
        // Retiring an untouched readahead page is not a workingset eviction:
        // it has never been consumed by the caller.  In particular, do not
        // let an older shadow survive this explicit retirement.
        if page.is_unused_prefetched() {
            clear_file_cache_shadows(&self.shared, [pn]);
        } else {
            self.record_eviction(pn);
        }
        Ok(())
    }

    pub(super) fn drain_cache(&self, file: &FileNode) -> VfsResult<()> {
        let _ = file;
        let native_mutation = begin_source_location_writeback_mutation(&self.inner)?;
        let _direct_guard = self.shared.direct_io_lock.lock();
        let mutation = self.begin_cache_invalidating_mutation()?;
        sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
            &self.inner,
            &self.shared,
            &mutation,
            held_native_writeback_gate(&native_mutation),
        )
    }

    // Cache-invalidation machinery for the in-progress write path.
    #[allow(dead_code)]
    pub(super) fn discard_cache(&self) -> VfsResult<()> {
        discard_cached_pages(&self.shared)
    }

    pub(super) fn sync_in_memory_cache(&self) {
        let Ok(file) = self.inner.entry().as_file() else {
            return;
        };
        if let Err(err) = self.flush_dirty_cache(file) {
            warn!("Failed to flush in-memory file cache: {err:?}");
        }
    }

    pub(super) fn flush_dirty_cache(&self, file: &FileNode) -> VfsResult<()> {
        flush_dirty_cache_shared(&self.shared, file)
    }

    /// Writes dirty cached pages intersecting one byte range. `len == 0`
    /// means through EOF. The shared cache/writeback locks serialize this
    /// selection with concurrent writers and make completion observable to a
    /// later range wait.
    pub(super) fn sync_range_marked(
        &self,
        offset: u64,
        len: u64,
        data_only: bool,
    ) -> Result<(), RangeSyncError> {
        // In-memory nodes have no backing device; range writeback is a no-op.
        if self.in_memory {
            return Ok(());
        }
        let file = self
            .inner
            .entry()
            .as_file()
            .map_err(RangeSyncError::Immediate)?;
        let native_mutation =
            begin_file_node_writeback_mutation(file).map_err(RangeSyncError::Immediate)?;
        let _direct_guard = self.shared.direct_io_lock.lock();
        let end = if len == 0 {
            u64::MAX
        } else {
            offset
                .checked_add(len)
                .ok_or(RangeSyncError::Immediate(VfsError::InvalidInput))?
        };
        let first = offset / PAGE_SIZE as u64;
        let last = if end == u64::MAX {
            u64::MAX
        } else {
            end.saturating_sub(1) / PAGE_SIZE as u64
        };
        let dirty_pages = {
            let guard = self.shared.page_cache.lock();
            guard
                .iter()
                .filter_map(|(pn, page)| {
                    (page.is_dirty() && (*pn as u64 >= first) && (*pn as u64 <= last))
                        .then_some(*pn)
                })
                .collect::<Vec<_>>()
        };
        let _range_lease = CachedFileShared::try_range_cache_lease(
            &self.shared,
            offset..end,
            RangeCacheLeaseKind::CachedWrite,
        )
        .map_err(RangeSyncError::Immediate)?;
        let _writeback_guard = self.shared.writeback_lock.read();
        flush_dirty_page_list_locked_with_held_native_gate(
            &self.shared,
            file,
            dirty_pages,
            true,
            held_native_writeback_gate(&native_mutation),
        )
        .map_err(RangeSyncError::Writeback)?;
        // sync_file_range writes selected dirty cache pages only.  In
        // particular it must not turn range writeback into fsync-like
        // metadata or device-cache persistence.
        let _ = data_only;
        Ok(())
    }

    pub fn sync_range(&self, offset: u64, len: u64, data_only: bool) -> VfsResult<()> {
        self.sync_range_marked(offset, len, data_only)
            .map_err(|error| match error {
                RangeSyncError::Immediate(error)
                | RangeSyncError::Writeback(DirtyWritebackError { error, .. }) => error,
            })
    }

    pub(super) fn complete_range_writeback(
        &self,
        result: Result<(), RangeSyncError>,
    ) -> VfsResult<()> {
        match result {
            Ok(()) => Ok(()),
            Err(RangeSyncError::Immediate(error)) => Err(error),
            Err(RangeSyncError::Writeback(error)) => {
                if error.worker_must_publish && !error.errseq_published {
                    publish_async_dirty_writeback_completion_error(
                        self.inner
                            .entry()
                            .as_file()
                            .expect("range writeback file changed type"),
                        error.error,
                    );
                }
                Err(error.error)
            }
        }
    }

    pub(super) fn range_writeback_snapshot(&self) -> RangeWritebackFence {
        let generation = {
            let mut queue = self.shared.range_writeback.queue.lock();
            let generation = queue.next_generation;
            *queue.interests.entry(generation).or_default() += 1;
            generation
        };
        RangeWritebackFence {
            shared: Some(self.shared.clone()),
            generation,
        }
    }

    pub(super) fn submit_range_writeback(
        &self,
        offset: u64,
        len: u64,
        data_only: bool,
    ) -> VfsResult<RangeWritebackFence> {
        let (generation, start_worker) = {
            let mut queue = self.shared.range_writeback.queue.lock();
            queue.next_generation = queue
                .next_generation
                .checked_add(1)
                .ok_or(VfsError::NoMemory)?;
            let generation = queue.next_generation;
            queue.pending.push_back(RangeWritebackRequest {
                generation,
                offset,
                len,
                data_only,
            });
            *queue.interests.entry(generation).or_default() += 1;
            let start = !queue.worker_running;
            if start {
                queue.worker_running = true;
            }
            (generation, start)
        };
        if start_worker {
            let worker = self.clone();
            if axtask::try_spawn_with_name(
                move || worker.range_writeback_worker(),
                alloc::string::String::from("range-writeback"),
            )
            .is_err()
            {
                let mut queue = self.shared.range_writeback.queue.lock();
                // Other submitters may have observed worker_running while the
                // task allocation was in progress. Complete every request in
                // that admission epoch with a definite error rather than
                // leaving their WAIT_* callers behind an orphaned FIFO.
                while let Some(request) = queue.pending.pop_front() {
                    queue.completed.push(RangeWritebackCompletion {
                        generation: request.generation,
                        offset: request.offset,
                        len: request.len,
                        result: Err(VfsError::NoMemory),
                    });
                }
                if let Some(count) = queue.interests.get_mut(&generation) {
                    *count -= 1;
                    if *count == 0 {
                        queue.interests.remove(&generation);
                    }
                }
                gc_range_writeback_completions(&mut queue);
                queue.worker_running = false;
                drop(queue);
                self.shared.range_writeback.completed.notify_all(false);
                return Err(VfsError::NoMemory);
            }
        }
        Ok(RangeWritebackFence {
            shared: Some(self.shared.clone()),
            generation,
        })
    }

    pub(super) fn range_writeback_worker(&self) {
        loop {
            let request = {
                let mut queue = self.shared.range_writeback.queue.lock();
                match queue.pending.pop_front() {
                    Some(request) => {
                        queue.active = Some((request.generation, request.offset, request.len));
                        request
                    }
                    None => {
                        queue.worker_running = false;
                        return;
                    }
                }
            };
            // The worker, rather than submitter, owns the synchronous lower
            // writeback.  The range/direct-I/O locks are taken inside this
            // call and are never held while the request waits in the FIFO.
            let result = self.complete_range_writeback(self.sync_range_marked(
                request.offset,
                request.len,
                request.data_only,
            ));
            let mut queue = self.shared.range_writeback.queue.lock();
            queue.active = None;
            queue.completed.push(RangeWritebackCompletion {
                generation: request.generation,
                offset: request.offset,
                len: request.len,
                result,
            });
            gc_range_writeback_completions(&mut queue);
            drop(queue);
            self.shared.range_writeback.completed.notify_all(false);
        }
    }

    pub(super) fn wait_range_writeback_through(
        &self,
        fence: &RangeWritebackFence,
        offset: u64,
        len: u64,
    ) -> VfsResult<()> {
        let generation = fence.generation;
        let overlaps = |start: u64, length: u64| {
            let end = if len == 0 {
                u64::MAX
            } else {
                offset.saturating_add(len)
            };
            let other_end = if length == 0 {
                u64::MAX
            } else {
                start.saturating_add(length)
            };
            start < end && offset < other_end
        };
        self.shared
            .range_writeback
            .completed
            .wait_until_interruptible(|| {
                let queue = self.shared.range_writeback.queue.lock();
                let active = queue.active.is_some_and(
                    |(request_generation, request_offset, request_len)| {
                        request_generation <= generation && overlaps(request_offset, request_len)
                    },
                );
                !active
                    && !queue.pending.iter().any(|request| {
                        request.generation <= generation && overlaps(request.offset, request.len)
                    })
            })
            .map_err(|_| VfsError::Interrupted)?;
        let queue = self.shared.range_writeback.queue.lock();
        let mut result = Ok(());
        for completion in &queue.completed {
            if completion.generation <= generation && overlaps(completion.offset, completion.len) {
                if let Err(error) = completion.result {
                    result = Err(error);
                    break;
                }
            }
        }
        drop(queue);
        result
    }

    pub(super) fn aligned_page_range(offset: u64, len: usize) -> Option<Range<u32>> {
        if len == 0 || offset % PAGE_SIZE as u64 != 0 || len % PAGE_SIZE != 0 {
            return None;
        }
        let end = offset.checked_add(u64::try_from(len).ok()?)?;
        let start = u32::try_from(offset / PAGE_SIZE as u64).ok()?;
        let end = u32::try_from(end / PAGE_SIZE as u64).ok()?;
        Some(start..end)
    }

    pub(super) fn range_has_cached_page(&self, pages: &Range<u32>) -> bool {
        let guard = self.shared.page_cache.lock();
        pages.clone().any(|pn| guard.contains(&pn))
    }

    /// Demotes resident cache pages in `pages` to the cold end of the LRU.
    /// Missing pages are deliberately left untouched: MADV_COLD must not
    /// fault or allocate file-cache entries.
    pub fn cold_pages(&self, pages: Range<u32>) -> VfsResult<usize> {
        let mut cache = self.shared.page_cache.lock();
        let mut demoted = 0usize;
        for pn in pages {
            if cache.contains(&pn) {
                cache.demote(&pn);
                demoted = demoted.checked_add(1).ok_or(VfsError::NoMemory)?;
            }
        }
        Ok(demoted)
    }

    /// Writes back and evicts resident file-cache pages in `pages`.
    ///
    /// This never creates pages.  In-memory files and pages that are pinned,
    /// under writeback, or blocked by a concurrent cache user are skipped;
    /// pageout is advisory and must not discard data merely to satisfy a
    /// reclaim hint.  Each page owns an invalidation transaction so earlier
    /// successful evictions remain committed if a later page fails.
    pub fn pageout_pages(&self, pages: Range<u32>) -> VfsResult<usize> {
        if self.in_memory {
            return Ok(0);
        }
        let native_mutation = begin_source_location_writeback_mutation(&self.inner)?;
        let _direct_guard = self.shared.direct_io_lock.lock();
        let mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(0),
            Err(error) => return Err(error),
        };
        let file = self.inner.entry().as_file()?;
        let mut evicted = 0usize;
        for pn in pages {
            let mut invalidation = CachedPageInvalidationTransaction::new_pageout(&mutation);
            if !invalidation.stage_page_for_pageout(pn)? {
                continue;
            }
            invalidation.prepare_evictions()?;
            invalidation.writeback_with_held_native_gate(
                file,
                true,
                held_native_writeback_gate(&native_mutation),
            )?;
            invalidation.commit_discard();
            evicted = evicted.checked_add(1).ok_or(VfsError::NoMemory)?;
        }
        release_cached_file_writeback_anchor_if_clean(&self.shared);
        Ok(evicted)
    }

    /// Reclaims one cold resident page through the same staged eviction
    /// transaction used by `MADV_PAGEOUT`.
    ///
    /// The method deliberately performs no work while an address-space lock
    /// is held by its caller.  Prepare publishes every mapping fence and
    /// write-protects aliases before backing I/O; commit or abort then wakes
    /// blocked mapping mutations through their reservation completion path.
    pub fn reclaim_one(&self) -> VfsResult<bool> {
        if self.in_memory {
            return Ok(false);
        }
        let native_mutation = begin_source_location_writeback_mutation(&self.inner)?;
        let _direct_guard = self.shared.direct_io_lock.lock();
        let (candidate, writeback_pending) = {
            let cache = self.shared.page_cache.lock();
            (
                cache.iter().rev().find_map(|(pn, page)| {
                    (!page.is_pinned() && !page.is_writeback()).then_some(*pn)
                }),
                cache.iter().any(|(_, page)| page.is_writeback()),
            )
        };
        let Some(pn) = candidate else {
            // A writeback pin is temporary. Faults must retry outside their
            // address-space lock instead of treating it as exhausted memory.
            return if writeback_pending {
                Err(VfsError::ResourceBusy)
            } else {
                Ok(false)
            };
        };
        // A retained pin on another page must not turn this fault into an
        // endless Busy retry. Fence only the chosen cached page, while still
        // excluding direct requests and short cache users during admission.
        let mutation = Self::begin_shared_cache_invalidating_range(
            &self.shared,
            page_range(u64::from(pn), 1),
        )?;
        let file = self.inner.entry().as_file()?;
        let mut invalidation = CachedPageInvalidationTransaction::new_pageout(&mutation);
        if !invalidation.stage_page_for_pageout(pn)? {
            return Ok(false);
        }
        invalidation.prepare_evictions()?;
        invalidation.writeback_with_held_native_gate(
            file,
            true,
            held_native_writeback_gate(&native_mutation),
        )?;
        invalidation.commit_discard();
        release_cached_file_writeback_anchor_if_clean(&self.shared);
        Ok(true)
    }

    pub(super) fn invalidate_cached_range_with_held_native_gate(
        &self,
        file: &FileNode,
        pages: Range<u32>,
        mutation: &CachedFileMutationGuard,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<usize> {
        let mut invalidation = CachedPageInvalidationTransaction::new(mutation);
        let count = invalidation.stage_range(pages)?;
        invalidation.prepare_evictions()?;
        invalidation.writeback_with_held_native_gate(file, true, native_gate)?;
        invalidation.commit_discard();
        record_cached_file_counter(&RANGE_INVALIDATE_PAGES, count as u64);
        Ok(count)
    }
}
