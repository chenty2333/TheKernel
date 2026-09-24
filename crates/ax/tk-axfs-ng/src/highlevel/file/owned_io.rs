//! Owned (asynchronous) file I/O: preparation, submission and completion.

use super::*;

/// Low-level interface for file operations.
#[derive(Clone)]
pub enum FileBackend {
    /// File I/O goes through the page cache.
    Cached(CachedFile),
    /// File I/O bypasses the page cache and hits the VFS directly.
    Direct(Location),
}

pub(super) const OWNED_CACHED_FILE_IO_BOUNCE_BYTES: usize = 256 * 1024;

/// A high-level prepared owned-I/O operation.  Unlike the raw provider
/// object, this wrapper owns the direct-I/O cache coherency transaction until
/// publication either succeeds or returns its request to the caller.
pub struct PreparedOwnedFileIo {
    pub(super) prepared: Option<PreparedFileIo>,
    pub(super) direct_coherency: Option<Arc<SleepingMutex<Option<DirectOwnedIoCoherency>>>>,
}

impl PreparedOwnedFileIo {
    pub fn is_nowait(&self) -> bool {
        self.prepared
            .as_ref()
            .expect("owned file I/O prepared request missing")
            .request()
            .policy()
            .nowait
    }

    pub(super) fn new(prepared: PreparedFileIo) -> Self {
        Self {
            prepared: Some(prepared),
            direct_coherency: None,
        }
    }

    pub(super) fn with_direct_coherency(
        prepared: PreparedFileIo,
        direct_coherency: Arc<SleepingMutex<Option<DirectOwnedIoCoherency>>>,
    ) -> Self {
        Self {
            prepared: Some(prepared),
            direct_coherency: Some(direct_coherency),
        }
    }

    /// Withdraws an unpublished operation without emitting a completion.
    /// Direct cache exclusion must end before the request owner becomes
    /// visible to a caller which may immediately retry it through another
    /// route.
    pub fn abort(mut self) -> (FileIoRequest, Box<dyn OwnedFileIoCompletion>) {
        let (request, completion) = self
            .prepared
            .take()
            .expect("owned file I/O prepared request missing")
            .abort();
        drop(
            self.direct_coherency
                .take()
                .and_then(|state| state.lock().take()),
        );
        (request, completion.into_retry_completion())
    }

    /// Publishes the exact previously prepared operation.  A publication
    /// failure restores the direct cache transaction before exposing the
    /// request for a later retry, so the returned owner never retains a stale
    /// cache exclusion lease.
    pub fn submit(mut self) -> Result<SubmittedFileIo, FileIoPrepareError> {
        if self
            .prepared
            .as_ref()
            .expect("owned file I/O prepared request missing")
            .request()
            .policy()
            .nowait
        {
            let (request, completion) = self.abort();
            return Err(FileIoPrepareError::new(
                VfsError::WouldBlock,
                request,
                completion,
            ));
        }
        match self
            .prepared
            .take()
            .expect("owned file I/O prepared request missing")
            .submit()
        {
            Ok(submitted) => Ok(submitted),
            Err(FileIoPrepareError {
                error,
                request,
                completion,
            }) => {
                drop(
                    self.direct_coherency
                        .take()
                        .and_then(|state| state.lock().take()),
                );
                Err(FileIoPrepareError::new(
                    error,
                    request,
                    completion.into_retry_completion(),
                ))
            }
        }
    }

    /// Executes a pre-admitted NOWAIT operation without publishing it.  The
    /// direct cache transaction is rolled back on a no-submit error.
    pub fn try_complete_immediate(mut self) -> Result<ImmediateFileIoResult, FileIoPrepareError> {
        if !self
            .prepared
            .as_ref()
            .expect("owned file I/O prepared request missing")
            .request()
            .policy()
            .nowait
        {
            let (request, completion) = self.abort();
            return Err(FileIoPrepareError::new(
                VfsError::InvalidInput,
                request,
                completion,
            ));
        }
        match self
            .prepared
            .take()
            .expect("owned file I/O prepared request missing")
            .try_complete_immediate()
        {
            Ok(result) => Ok(result),
            Err(FileIoPrepareError {
                error,
                request,
                completion,
            }) => {
                drop(
                    self.direct_coherency
                        .take()
                        .and_then(|state| state.lock().take()),
                );
                Err(FileIoPrepareError::new(
                    error,
                    request,
                    completion.into_retry_completion(),
                ))
            }
        }
    }
}

/// Exact cache owners retained for one published direct request.  The page
/// transaction is committed before the provider can observe the request; the
/// range lease remains live until its one terminal completion releases it.
pub(super) struct DirectOwnedIoCoherency {
    pub(super) cache_exclusion: Option<DirectOwnedIoExclusion>,
    pub(super) invalidation: Option<CachedPageInvalidationTransaction>,
    /// This is deliberately not a cache exclusion: it serializes native
    /// fileattr publication with the accepted write and must cover the
    /// metadata/sync completion too.
    // Held only for its Drop semantics; read by the in-progress write path.
    #[allow(dead_code)]
    pub(super) native_mutation: Option<FileAttrMutationGuard>,
}

impl DirectOwnedIoCoherency {
    /// Releases only resources which would make a terminal callback's own
    /// cache flush or retry conflict with this completed direct operation.
    /// The fileattr mutation guard is retained until that callback returns.
    pub(super) fn release_cache_exclusion(&mut self) {
        drop(self.invalidation.take());
        drop(self.cache_exclusion.take());
    }
}

// The payloads are cache-ownership guards held only for their Drop semantics.
#[allow(dead_code)]
pub(super) enum DirectOwnedIoExclusion {
    Range(RangeCacheLease),
    WholeFile(CachedFileMutationGuard),
}

/// Completion adapter that releases direct-I/O cache ownership before the
/// kernel sees the terminal result.  Preparation and publication failures
/// retain the original request/completion pair without manufacturing a CQE.
pub(super) struct DirectOwnedIoCompletion {
    pub(super) time_completion: Option<Box<OwnedFileIoTimeCompletion>>,
    pub(super) coherency: Arc<SleepingMutex<Option<DirectOwnedIoCoherency>>>,
}

/// High-level metadata completion for an accepted operation.  Cache and
/// provider workers cannot borrow `File`, so the OFD's resolved location and
/// immutable direction are retained with the terminal owner instead.
pub(super) struct OwnedFileIoTimeCompletion {
    pub(super) completion: Option<Box<dyn OwnedFileIoCompletion>>,
    pub(super) location: Location,
    pub(super) backend: FileBackend,
    pub(super) open_handle: Option<Arc<dyn FileNodeOps>>,
    pub(super) opcode: FileIoOpcode,
    pub(super) policy: FileIoPolicy,
    pub(super) update_atime: bool,
}

impl OwnedFileIoTimeCompletion {
    /// Performs the OFD-owned terminal work but deliberately leaves delivery
    /// to the original completion sink to the caller.  Direct I/O needs that
    /// boundary to release its native mutation token after metadata/sync, yet
    /// before a user sink can synchronously retry the operation.
    pub(super) fn finish_metadata(&self, completion: &mut FileIoCompletion) {
        let completed =
            matches!(completion.result, ImmediateFileIoResult::Completed(bytes) if bytes != 0);
        #[cfg(feature = "times")]
        if completed && !self.policy.nowait {
            let mut update = MetadataUpdate::default();
            match self.opcode {
                FileIoOpcode::Read if self.update_atime => {
                    update.atime = Some(axhal::time::wall_time().into())
                }
                FileIoOpcode::Write => {
                    let now = axhal::time::wall_time().into();
                    update.mtime = Some(now);
                    update.ctime = Some(now);
                }
                _ => {}
            }
            if let Err(error) = self.location.update_supported_metadata(update) {
                // Timestamp persistence is best-effort for ordinary I/O: a
                // provider metadata error must not rewrite already completed
                // data I/O. NOWAIT never reaches this blocking branch.
                debug!("owned file I/O timestamp update failed: {error:?}");
            }
        }
        if completed
            && self.opcode == FileIoOpcode::Write
            && self.policy.sync != FileIoSyncMode::None
        {
            let data_only = self.policy.sync == FileIoSyncMode::Data;
            let sync_result = match self.open_handle.as_deref() {
                Some(handle) => handle.sync(data_only),
                None => match completion.result {
                    ImmediateFileIoResult::Completed(bytes) => {
                        self.backend
                            .sync_range(completion.actual_offset, bytes as u64, data_only)
                    }
                    _ => Ok(()),
                },
            };
            if let Err(error) = sync_result {
                completion.result = ImmediateFileIoResult::Failed(error);
            }
        }
        if completed && self.policy.dontcache {
            if let ImmediateFileIoResult::Completed(bytes) = completion.result {
                // Advisory failure cannot revise an already completed I/O.
                let _ = self
                    .backend
                    .fadvise_dontneed(completion.actual_offset, bytes as u64);
            }
        }
    }

    pub(super) fn into_original_completion(mut self: Box<Self>) -> Box<dyn OwnedFileIoCompletion> {
        self.completion
            .take()
            .expect("owned file I/O terminal completion missing")
    }

    pub(super) fn finish_metadata_and_take_completion(
        self: Box<Self>,
        mut completion: FileIoCompletion,
    ) -> (Box<dyn OwnedFileIoCompletion>, FileIoCompletion) {
        self.finish_metadata(&mut completion);
        (self.into_original_completion(), completion)
    }
}

impl OwnedFileIoCompletion for OwnedFileIoTimeCompletion {
    fn complete(self: Box<Self>, completion: FileIoCompletion) {
        let (completion_callback, completion) =
            self.finish_metadata_and_take_completion(completion);
        completion_callback.complete(completion);
    }

    fn into_retry_completion(self: Box<Self>) -> Box<dyn OwnedFileIoCompletion> {
        self.into_original_completion()
    }
}

/// A completed zero-length request.  It deliberately never opens a provider
/// queue or acquires a cache/direct range lease.
pub(super) struct ZeroLengthOwnedIoSubmission;

impl PreparedFileIoSubmission for ZeroLengthOwnedIoSubmission {
    fn publish(
        self: Box<Self>,
        payload: FileIoPublishPayload,
    ) -> Result<SubmittedFileIo, FileIoPublishError> {
        payload
            .commit()
            .complete(ImmediateFileIoResult::Completed(0));
        let control: Box<dyn SubmittedFileIoControl> = self;
        Ok(SubmittedFileIo::new(control))
    }

    fn try_complete_immediate(
        self: Box<Self>,
        _request: &mut dyn FileIoRequestAccess,
    ) -> VfsResult<ImmediateFileIoResult> {
        Ok(ImmediateFileIoResult::Completed(0))
    }
}

impl SubmittedFileIoControl for ZeroLengthOwnedIoSubmission {
    fn cancel(self: Box<Self>) -> FileIoCancelOutcome {
        FileIoCancelOutcome::Terminal
    }
}

/// Direct providers must hand us a retained, entirely try-only permit before
/// a NOWAIT request can enter their queue.  Until a provider implements that
/// contract, expose the honest immediate EAGAIN path rather than reserving
/// provider state or beginning cache invalidation during preparation.
pub(super) struct NowaitUnavailableOwnedIoSubmission;

impl PreparedFileIoSubmission for NowaitUnavailableOwnedIoSubmission {
    fn publish(
        self: Box<Self>,
        payload: FileIoPublishPayload,
    ) -> Result<SubmittedFileIo, FileIoPublishError> {
        payload
            .commit()
            .complete(ImmediateFileIoResult::Failed(VfsError::WouldBlock));
        let control: Box<dyn SubmittedFileIoControl> = self;
        Ok(SubmittedFileIo::new(control))
    }

    fn try_complete_immediate(
        self: Box<Self>,
        _request: &mut dyn FileIoRequestAccess,
    ) -> VfsResult<ImmediateFileIoResult> {
        Ok(ImmediateFileIoResult::Failed(VfsError::WouldBlock))
    }
}

impl SubmittedFileIoControl for NowaitUnavailableOwnedIoSubmission {
    fn cancel(self: Box<Self>) -> FileIoCancelOutcome {
        FileIoCancelOutcome::Terminal
    }
}

pub(super) fn prepare_nowait_unavailable_owned_file_io(
    request: FileIoRequest,
    completion: Box<dyn OwnedFileIoCompletion>,
) -> Result<PreparedOwnedFileIo, FileIoPrepareError> {
    // This ZST reservation never owns request/completion, so a fallible
    // allocation can return their exact original pair without a staging copy.
    let submission: Box<dyn PreparedFileIoSubmission> =
        match Box::try_new(NowaitUnavailableOwnedIoSubmission) {
            Ok(submission) => submission,
            Err(_) => {
                return Err(FileIoPrepareError::new(
                    VfsError::NoMemory,
                    request,
                    completion,
                ));
            }
        };
    Ok(PreparedOwnedFileIo::new(PreparedFileIo::new(
        request, completion, submission,
    )))
}

impl OwnedFileIoCompletion for DirectOwnedIoCompletion {
    fn complete(mut self: Box<Self>, completion: FileIoCompletion) {
        // The terminal callback may synchronously flush or retry this same
        // file (RWF_SYNC/RWF_DSYNC and append retry both do so).  It must not
        // observe this request's range lease or staged invalidation as a
        // conflicting live operation.  Native fileattr admission is different:
        // it must remain held through mtime/ctime and sync completion so a
        // setter cannot publish conflicting attributes between the accepted
        // write and its metadata commit.
        {
            let mut coherency = self.coherency.lock();
            if let Some(coherency) = coherency.as_mut() {
                coherency.release_cache_exclusion();
            }
        }
        // The OFD's timestamp and sync work belongs to the accepted write,
        // so it runs while native fileattr admission is still stable.
        let time_completion = self
            .time_completion
            .take()
            .expect("direct owned file I/O completion missing");
        let (completion_callback, completion) =
            time_completion.finish_metadata_and_take_completion(completion);
        // Only the native mutation guard remains.  It is released after the
        // internal metadata/sync path but before the user sink, whose retry
        // path is allowed to enter a fresh native mutation transaction.
        drop(self.coherency.lock().take());
        completion_callback.complete(completion);
    }

    fn into_retry_completion(mut self: Box<Self>) -> Box<dyn OwnedFileIoCompletion> {
        self.time_completion
            .take()
            .expect("direct owned file I/O completion missing")
            .into_original_completion()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CachedOwnedIoPhase {
    Prepared,
    Published,
    Claimed,
    Terminal,
}

pub(super) struct CachedOwnedIoState {
    pub(super) phase: CachedOwnedIoPhase,
    pub(super) payload: Option<axfs_ng_vfs::PublishedFileIoPayload>,
}

/// Both buffers are reserved before publication. `transfer` is the sole
/// temporary alias of the owned user buffer; `cache_page` lets page-cache
/// helpers copy cache data without allocating after the request is live.
pub(super) struct CachedOwnedIoBounces {
    pub(super) transfer: Box<[u8]>,
    pub(super) cache_page: Box<[u8]>,
}

/// Cache-owned async submission.  It never routes a buffered OFD through a
/// provider's direct queue: the worker invokes `CachedFile` and therefore
/// preserves the page-cache/dirty-page semantics of the opened description.
pub(super) struct CachedOwnedIoSubmission {
    pub(super) cache: CachedFile,
    pub(super) state: Arc<SleepingMutex<CachedOwnedIoState>>,
    pub(super) bounce: Arc<SleepingMutex<CachedOwnedIoBounces>>,
    pub(super) task_name: Option<alloc::string::String>,
    pub(super) update_atime: bool,
}

impl CachedOwnedIoSubmission {
    pub(super) fn prepare(
        cache: CachedFile,
        request: FileIoRequest,
        completion: Box<dyn OwnedFileIoCompletion>,
        update_atime: bool,
    ) -> Result<PreparedFileIo, FileIoPrepareError> {
        // A zero-length operation uses the dedicated terminal submission and
        // therefore retains a true zero-length bounce. Every nonempty worker
        // owns at least one page so cache reads/writes cannot allocate later.
        let bounce_len = request
            .len()
            .min(OWNED_CACHED_FILE_IO_BOUNCE_BYTES)
            .max(PAGE_SIZE);
        let mut bounce = Vec::new();
        if bounce.try_reserve_exact(bounce_len).is_err() {
            return Err(FileIoPrepareError::new(
                VfsError::NoMemory,
                request,
                completion,
            ));
        }
        bounce.resize(bounce_len, 0);
        let mut task_name = alloc::string::String::new();
        if task_name
            .try_reserve_exact("cached-owned-file-io".len())
            .is_err()
        {
            return Err(FileIoPrepareError::new(
                VfsError::NoMemory,
                request,
                completion,
            ));
        }
        task_name.push_str("cached-owned-file-io");
        let state = match Arc::try_new(SleepingMutex::new(CachedOwnedIoState {
            phase: CachedOwnedIoPhase::Prepared,
            payload: None,
        })) {
            Ok(state) => state,
            Err(_) => {
                return Err(FileIoPrepareError::new(
                    VfsError::NoMemory,
                    request,
                    completion,
                ));
            }
        };
        let mut cache_page = Vec::new();
        if cache_page.try_reserve_exact(PAGE_SIZE).is_err() {
            return Err(FileIoPrepareError::new(
                VfsError::NoMemory,
                request,
                completion,
            ));
        }
        cache_page.resize(PAGE_SIZE, 0);
        let bounce = match Arc::try_new(SleepingMutex::new(CachedOwnedIoBounces {
            transfer: bounce.into_boxed_slice(),
            cache_page: cache_page.into_boxed_slice(),
        })) {
            Ok(bounce) => bounce,
            Err(_) => {
                return Err(FileIoPrepareError::new(
                    VfsError::NoMemory,
                    request,
                    completion,
                ));
            }
        };
        let submission = match Box::try_new(Self {
            cache,
            state,
            bounce,
            task_name: Some(task_name),
            update_atime,
        }) {
            Ok(submission) => submission,
            Err(_) => {
                return Err(FileIoPrepareError::new(
                    VfsError::NoMemory,
                    request,
                    completion,
                ));
            }
        };
        Ok(PreparedFileIo::new(request, completion, submission))
    }

    pub(super) fn run(
        cache: CachedFile,
        state: Arc<SleepingMutex<CachedOwnedIoState>>,
        bounce: Arc<SleepingMutex<CachedOwnedIoBounces>>,
    ) {
        let Some(mut payload) = ({
            let mut state_guard = state.lock();
            if state_guard.phase != CachedOwnedIoPhase::Published {
                None
            } else {
                state_guard.phase = CachedOwnedIoPhase::Claimed;
                state_guard.payload.take()
            }
        }) else {
            return;
        };

        let outcome = {
            let mut bounce = bounce.lock();
            cached_owned_file_io_execute(&cache, &mut payload, &mut bounce)
        };
        {
            let mut state_guard = state.lock();
            debug_assert_eq!(state_guard.phase, CachedOwnedIoPhase::Claimed);
            state_guard.phase = CachedOwnedIoPhase::Terminal;
        }
        payload.complete_at(outcome.result, outcome.actual_offset);
        // Keep a normal cached write's admission through the metadata
        // completion callback just as direct owned I/O does.
        drop(outcome.native_mutation);
    }

    pub(super) fn try_complete_nowait(
        cache: &CachedFile,
        request: &mut dyn FileIoRequestAccess,
        bounces: &mut CachedOwnedIoBounces,
        update_atime: bool,
    ) -> VfsResult<ImmediateFileIoResult> {
        if request.len() == 0 {
            return Ok(ImmediateFileIoResult::Completed(0));
        }
        #[cfg(not(feature = "times"))]
        let _ = update_atime;
        // Keep this narrow, fully try-locked cache-hit operation.  Larger
        // ranges require multi-page rollback/preflight ownership and must
        // return EAGAIN rather than expose a partially mutated cache.
        if request.len() > PAGE_SIZE {
            return Err(VfsError::WouldBlock);
        }
        #[cfg(feature = "times")]
        let time_update = match request.opcode() {
            FileIoOpcode::Read if update_atime => MetadataUpdate {
                atime: Some(axhal::time::wall_time().into()),
                ..MetadataUpdate::default()
            },
            FileIoOpcode::Write => {
                let now = axhal::time::wall_time().into();
                MetadataUpdate {
                    mtime: Some(now),
                    ctime: Some(now),
                    ..MetadataUpdate::default()
                }
            }
            _ => MetadataUpdate::default(),
        };
        #[cfg(feature = "times")]
        let time_reservation = if time_update.is_empty() {
            None
        } else {
            let needed = match request.opcode() {
                FileIoOpcode::Read => axfs_ng_vfs::MetadataUpdateCapabilities::ATIME,
                FileIoOpcode::Write => {
                    axfs_ng_vfs::MetadataUpdateCapabilities::MTIME
                        | axfs_ng_vfs::MetadataUpdateCapabilities::CTIME
                }
            };
            if !cache
                .inner
                .filesystem()
                .metadata_update_capabilities()
                .contains(needed)
            {
                return Err(VfsError::WouldBlock);
            }
            let Some(data) = cache.inner.entry().persistent_user_data() else {
                return Err(VfsError::WouldBlock);
            };
            let overlay = data.try_get_or_install_metadata_time_overlay()?;
            Some(overlay.try_reserve().ok_or(VfsError::WouldBlock)?)
        };
        let offset = request.offset();
        let end = offset
            .checked_add(request.len() as u64)
            .ok_or(VfsError::InvalidInput)?;
        let page_number =
            u32::try_from(offset / PAGE_SIZE as u64).map_err(|_| VfsError::InvalidInput)?;
        if end > (u64::from(page_number) + 1) * PAGE_SIZE as u64 {
            return Err(VfsError::WouldBlock);
        }
        let Some(_direct_guard) = cache.shared.direct_io_lock.try_lock() else {
            return Err(VfsError::WouldBlock);
        };
        let append_guard = match request.opcode() {
            FileIoOpcode::Read => cache.shared.append_lock.try_read(),
            FileIoOpcode::Write if request.placement() == FileIoWritePlacement::Positioned => {
                cache.shared.append_lock.try_read()
            }
            FileIoOpcode::Write => return Err(VfsError::WouldBlock),
        };
        let Some(_append_guard) = append_guard else {
            return Err(VfsError::WouldBlock);
        };

        let result = match request.opcode() {
            FileIoOpcode::Read => {
                let Some(mut cache_guard) = cache.shared.page_cache.try_lock() else {
                    return Err(VfsError::WouldBlock);
                };
                let Some(page) = cache_guard.get_mut(&page_number) else {
                    return Err(VfsError::WouldBlock);
                };
                if page.is_writeback() {
                    return Err(VfsError::WouldBlock);
                }
                let page_offset = (offset % PAGE_SIZE as u64) as usize;
                bounces.transfer[..request.len()]
                    .copy_from_slice(&page.data()[page_offset..page_offset + request.len()]);
                file_cache_record_page_reference(page);
                drop(cache_guard);
                let copied = request.destination_copy_at(0, &bounces.transfer[..request.len()])?;
                if copied > request.len() {
                    return Err(VfsError::Io);
                }
                Ok(ImmediateFileIoResult::Completed(copied))
            }
            FileIoOpcode::Write => {
                // Re-read native flags under the provider's try-only lock at
                // the actual mutation point. A prepare-time result must not
                // be cached across concurrent file_setattr/ioctl changes.
                let _native_mutation =
                    try_begin_native_location_mutation_nowait(&cache.inner, false)?;
                if !cache.shared.nowait_write_within_known_len(end) {
                    return Err(VfsError::WouldBlock);
                }
                // Fully copy before the page is made mutable; a usercopy
                // fault can therefore never leave a NOWAIT partial write.
                let copied = request.source_copy_at(0, &mut bounces.transfer[..request.len()])?;
                if copied != request.len() {
                    return Err(VfsError::WouldBlock);
                }
                let Some(mut cache_guard) = cache.shared.page_cache.try_lock() else {
                    return Err(VfsError::WouldBlock);
                };
                let Some(page) = cache_guard.get_mut(&page_number) else {
                    return Err(VfsError::WouldBlock);
                };
                if page.is_writeback() || page.is_pinned() {
                    return Err(VfsError::WouldBlock);
                }
                // Every fallible preflight step (usercopy and all cache
                // permits) is complete.  Install the dirty-page owner only
                // now; the guard removes a newly installed owner if a future
                // pre-mutation edge is added and returns early.
                let anchor = try_retain_cached_file_writeback_anchor(&cache.inner, &cache.shared)
                    .ok_or(VfsError::WouldBlock)?;
                let page_offset = (offset % PAGE_SIZE as u64) as usize;
                page.data()[page_offset..page_offset + request.len()]
                    .copy_from_slice(&bounces.transfer[..request.len()]);
                page.mark_dirty();
                anchor.commit();
                drop(cache_guard);
                Ok(ImmediateFileIoResult::Completed(request.len()))
            }
        };
        #[cfg(feature = "times")]
        if !time_update.is_empty()
            && matches!(result, Ok(ImmediateFileIoResult::Completed(bytes)) if bytes != 0)
        {
            let reservation =
                time_reservation.expect("timestamp reservation admitted before NOWAIT I/O");
            let mut snapshot = reservation.pending_snapshot();
            snapshot.merge_update(time_update);
            reservation.publish(snapshot);
        }
        result
    }
}

impl PreparedFileIoSubmission for CachedOwnedIoSubmission {
    fn publish(
        mut self: Box<Self>,
        payload: FileIoPublishPayload,
    ) -> Result<SubmittedFileIo, FileIoPublishError> {
        {
            let mut state = self.state.lock();
            debug_assert_eq!(state.phase, CachedOwnedIoPhase::Prepared);
            state.payload = Some(payload.commit());
            state.phase = CachedOwnedIoPhase::Published;
        }
        let state = self.state.clone();
        let bounce = self.bounce.clone();
        let cache = self.cache.clone();
        let task_name = self
            .task_name
            .take()
            .expect("cached owned file I/O task name missing");
        if axtask::try_spawn_with_name(move || Self::run(cache, state, bounce), task_name).is_err()
        {
            let payload = {
                let mut state = self.state.lock();
                debug_assert_eq!(state.phase, CachedOwnedIoPhase::Published);
                state.phase = CachedOwnedIoPhase::Terminal;
                state
                    .payload
                    .take()
                    .expect("cached owned file I/O payload missing")
            };
            payload.complete(ImmediateFileIoResult::Failed(VfsError::NoMemory));
            let control: Box<dyn SubmittedFileIoControl> = self;
            return Ok(SubmittedFileIo::new(control));
        }
        let control: Box<dyn SubmittedFileIoControl> = self;
        Ok(SubmittedFileIo::new(control))
    }

    fn try_complete_immediate(
        self: Box<Self>,
        request: &mut dyn FileIoRequestAccess,
    ) -> VfsResult<ImmediateFileIoResult> {
        let Some(mut bounces) = self.bounce.try_lock() else {
            return Err(VfsError::WouldBlock);
        };
        Self::try_complete_nowait(&self.cache, request, &mut bounces, self.update_atime)
    }
}

impl SubmittedFileIoControl for CachedOwnedIoSubmission {
    fn cancel(self: Box<Self>) -> FileIoCancelOutcome {
        let payload = {
            let mut state = self.state.lock();
            match state.phase {
                CachedOwnedIoPhase::Published => {
                    state.phase = CachedOwnedIoPhase::Terminal;
                    state.payload.take()
                }
                CachedOwnedIoPhase::Claimed => return FileIoCancelOutcome::InFlight,
                CachedOwnedIoPhase::Prepared | CachedOwnedIoPhase::Terminal => {
                    return FileIoCancelOutcome::Terminal;
                }
            }
        };
        if let Some(payload) = payload {
            payload.complete(ImmediateFileIoResult::Cancelled);
            FileIoCancelOutcome::Cancelled
        } else {
            FileIoCancelOutcome::Terminal
        }
    }
}

pub(super) fn owned_file_io_prefix_or_error(done: usize, error: VfsError) -> VfsResult<usize> {
    if done == 0 { Err(error) } else { Ok(done) }
}

pub(super) struct CachedOwnedIoOutcome {
    pub(super) result: ImmediateFileIoResult,
    pub(super) actual_offset: u64,
    pub(super) native_mutation: Option<FileAttrMutationGuard>,
}

pub(super) fn cached_owned_file_io_execute(
    cache: &CachedFile,
    payload: &mut axfs_ng_vfs::PublishedFileIoPayload,
    bounces: &mut CachedOwnedIoBounces,
) -> CachedOwnedIoOutcome {
    let mut actual_offset = 0;
    let mut native_mutation = None;
    let result = payload.with_request(|request| {
        actual_offset = request.offset();
        match request.opcode() {
            FileIoOpcode::Read => cached_owned_read_execute(cache, request, bounces),
            FileIoOpcode::Write => match request.placement() {
                FileIoWritePlacement::Positioned => {
                    native_mutation = begin_native_location_mutation(&cache.inner, false)?;
                    let _direct_guard = cache.shared.direct_io_lock.lock();
                    let _append_guard = cache.shared.append_lock.read();
                    let offset = request.offset();
                    cached_owned_write_execute(cache, request, offset, bounces)
                }
                FileIoWritePlacement::Append => {
                    // EOF is selected only after the append domain is held, and
                    // remains fixed for every chunk of this operation.
                    native_mutation = begin_native_location_mutation(&cache.inner, true)?;
                    let _direct_guard = cache.shared.direct_io_lock.lock();
                    let _append_guard = cache.shared.append_lock.write();
                    let offset = cache.inner.entry().as_file()?.len()?;
                    actual_offset = offset;
                    if let Some(alignment) = request.policy().direct_offset_alignment
                        && !offset.is_multiple_of(alignment as u64)
                    {
                        return Err(VfsError::InvalidInput);
                    }
                    cached_owned_write_execute(cache, request, offset, bounces)
                }
            },
        }
    });
    CachedOwnedIoOutcome {
        result: match result {
            Ok(done) => ImmediateFileIoResult::Completed(done),
            Err(error) => ImmediateFileIoResult::Failed(error),
        },
        actual_offset,
        native_mutation,
    }
}

pub(super) fn cached_owned_read_execute(
    cache: &CachedFile,
    request: &mut dyn FileIoRequestAccess,
    bounces: &mut CachedOwnedIoBounces,
) -> VfsResult<usize> {
    let mut done = 0usize;
    while done < request.len() {
        let length = (request.len() - done).min(bounces.transfer.len());
        let offset = request
            .offset()
            .checked_add(done as u64)
            .ok_or(VfsError::InvalidInput)?;
        let read = match cache.read_at_sync_with_bounce(
            &mut bounces.transfer[..length],
            offset,
            &mut bounces.cache_page,
        ) {
            Ok(read) => read,
            Err(error) => return owned_file_io_prefix_or_error(done, error),
        };
        if read == 0 {
            break;
        }
        let copied = match request.destination_copy_at(done, &bounces.transfer[..read]) {
            Ok(copied) => copied,
            Err(error) => return owned_file_io_prefix_or_error(done, error),
        };
        if copied > read {
            return Err(VfsError::Io);
        }
        done = done.checked_add(copied).ok_or(VfsError::Io)?;
        if copied < read {
            break;
        }
    }
    Ok(done)
}

pub(super) fn cached_owned_write_execute(
    cache: &CachedFile,
    request: &mut dyn FileIoRequestAccess,
    offset: u64,
    bounces: &mut CachedOwnedIoBounces,
) -> VfsResult<usize> {
    let mut done = 0usize;
    while done < request.len() {
        let length = (request.len() - done).min(bounces.transfer.len());
        let copied = match request.source_copy_at(done, &mut bounces.transfer[..length]) {
            Ok(copied) => copied,
            Err(error) => return owned_file_io_prefix_or_error(done, error),
        };
        if copied == 0 {
            return owned_file_io_prefix_or_error(done, VfsError::WriteZero);
        }
        let current = offset
            .checked_add(done as u64)
            .ok_or(VfsError::InvalidInput)?;
        let written = match cache.write_at_locked_with_bounce(
            &bounces.transfer[..copied],
            current,
            &mut bounces.cache_page,
        ) {
            Ok(written) => written,
            Err(error) => return owned_file_io_prefix_or_error(done, error),
        };
        if written > copied {
            return Err(VfsError::Io);
        }
        if written == 0 {
            break;
        }
        done = done.checked_add(written).ok_or(VfsError::Io)?;
        if written < copied {
            break;
        }
    }
    if done != 0 {
        retain_cached_file_writeback_anchor_if_dirty(&cache.inner, &cache.shared);
    }
    Ok(done)
}

pub(super) fn prepare_direct_owned_io_coherency(
    location: &Location,
    opcode: FileIoOpcode,
    placement: FileIoWritePlacement,
    offset: u64,
    length: usize,
) -> VfsResult<DirectOwnedIoCoherency> {
    let native_mutation = if opcode == FileIoOpcode::Write {
        begin_native_location_mutation(location, placement == FileIoWritePlacement::Append)?
    } else {
        begin_source_location_writeback_mutation(location)?
    };
    if opcode == FileIoOpcode::Write && placement == FileIoWritePlacement::Append {
        let shared = cached_file_shared_for_location_or_create(location);
        let _direct_guard = shared.direct_io_lock.lock();
        let mutation = CachedFile::begin_shared_cache_invalidating_mutation(&shared)?;
        sync_and_invalidate_cached_file_pages_locked_with_held_native_gate(
            location,
            &shared,
            &mutation,
            held_native_writeback_gate(&native_mutation),
        )?;
        return Ok(DirectOwnedIoCoherency {
            cache_exclusion: Some(DirectOwnedIoExclusion::WholeFile(mutation)),
            invalidation: None,
            native_mutation,
        });
    }
    let end = offset
        .checked_add(u64::try_from(length).map_err(|_| VfsError::InvalidInput)?)
        .ok_or(VfsError::InvalidInput)?;
    let shared = cached_file_shared_for_location_or_create(location);
    let range_lease = CachedFileShared::try_range_cache_lease(
        &shared,
        offset..end,
        match opcode {
            FileIoOpcode::Read => RangeCacheLeaseKind::DirectRead,
            FileIoOpcode::Write => RangeCacheLeaseKind::DirectWrite,
        },
    )?;
    let invalidation = {
        let _direct_guard = shared.direct_io_lock.lock();
        let _writeback_guard = shared.writeback_lock.write();
        wait_for_all_writeback_clear(&shared);
        let file = location.entry().as_file()?;
        let first_page =
            u32::try_from(offset / PAGE_SIZE as u64).map_err(|_| VfsError::InvalidInput)?;
        let last_page =
            u32::try_from(end.div_ceil(PAGE_SIZE as u64)).map_err(|_| VfsError::InvalidInput)?;
        let mut invalidation = CachedPageInvalidationTransaction::new_shared(shared.clone());
        invalidation.stage_range(first_page..last_page)?;
        invalidation.prepare_evictions()?;
        invalidation.writeback_with_held_native_gate(
            file,
            true,
            held_native_writeback_gate(&native_mutation),
        )?;
        invalidation
    };
    // From this point the exact range lease is retained by the terminal
    // completion.  A dropped/pre-publication operation instead drops this
    // committed transaction and lease together; no old page can reappear
    // while a provider may access the direct range.
    invalidation.commit_discard();
    Ok(DirectOwnedIoCoherency {
        cache_exclusion: Some(DirectOwnedIoExclusion::Range(range_lease)),
        invalidation: None,
        native_mutation,
    })
}

/// Per-open-file-description advice bits selected by POSIX_FADV_*.
///
/// These are deliberately independent: RANDOM suppresses automatic
/// readahead, SEQUENTIAL extends its window, and NOREUSE changes reclamation
/// treatment for pages actually consumed by later reads. NORMAL clears all
/// three, matching Linux's reset behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FadviseReadahead {
    Normal     = 0,
    Random     = 1,
    Sequential = 2,
    NoReuse    = 3,
}

pub(super) const FADVISE_RANDOM: u8 = 1 << 0;
pub(super) const FADVISE_SEQUENTIAL: u8 = 1 << 1;
pub(super) const FADVISE_NOREUSE: u8 = 1 << 2;

#[inline]
pub(super) const fn fadvise_next_bits(previous: u8, set: u8, clear: u8) -> u8 {
    (previous | set) & !clear
}
