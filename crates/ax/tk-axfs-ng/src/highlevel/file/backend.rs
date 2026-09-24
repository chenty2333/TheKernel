//! FileBackend: dispatch between cached and direct I/O.

use super::*;

impl FileBackend {
    pub(super) fn append_vectored_with_held_native_mutation(
        &self,
        src: &[&[u8]],
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<(usize, u64)> {
        match self {
            Self::Cached(cached) => {
                cached.append_vectored_with_held_native_mutation(src, native_gate)
            }
            Self::Direct(loc) => with_cache_invalidating_file_operation_with_held_native(
                loc,
                native_gate,
                |shared, file| {
                    let _append_guard = shared.append_lock.write();
                    let mut total = 0usize;
                    let mut end = file.len()?;
                    for buf in src.iter().copied() {
                        if buf.is_empty() {
                            continue;
                        }
                        let requested = buf.len();
                        let mut element_written = 0usize;
                        while element_written < requested {
                            let limit = (requested - element_written).min(Self::DIRECT_IO_CHUNK);
                            let chunk = &buf[element_written..element_written + limit];
                            let (written, new_end) = match file.append(chunk) {
                                Ok(result) => result,
                                Err(_) if total != 0 => break,
                                Err(error) => return Err(error),
                            };
                            total += written;
                            element_written += written;
                            end = new_end;
                            if written < limit || written == 0 {
                                break;
                            }
                        }
                        if element_written < requested || element_written == 0 {
                            break;
                        }
                    }
                    Ok((total, end))
                },
            ),
        }
    }

    pub(super) fn append_with_admission_with_held_native_mutation(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<(usize, u64)> {
        match self {
            Self::Cached(cached) => {
                cached.append_with_admission_with_held_native_mutation(src, admit, native_gate)
            }
            Self::Direct(loc) => with_cache_invalidating_file_operation_with_held_native(
                loc,
                native_gate,
                |shared, file| {
                    let _append_guard = shared.append_lock.write();
                    let mut src = src;
                    let mut total = 0;
                    let mut end = file.len()?;
                    let requested = src.remaining();
                    let allowed = admit(end, requested)?;
                    if allowed > requested {
                        return Err(VfsError::InvalidInput);
                    }
                    let mut admitted = (&mut src).take(allowed as u64);
                    let mut chunk = vec![0_u8; Self::DIRECT_IO_CHUNK];
                    while admitted.remaining() > 0 {
                        let limit = admitted.remaining().min(chunk.len());
                        let read = match admitted.read(&mut chunk[..limit]) {
                            Ok(read) => read,
                            Err(_) if total != 0 => break,
                            Err(error) => return Err(error),
                        };
                        if read == 0 {
                            break;
                        }
                        let (written, new_end) = match file.append(&chunk[..read]) {
                            Ok(result) => result,
                            Err(_) if total != 0 => break,
                            Err(error) => return Err(error),
                        };
                        if written == 0 {
                            break;
                        }
                        total += written;
                        end = new_end;
                        if written < read {
                            break;
                        }
                    }
                    Ok((total, end))
                },
            ),
        }
    }

    pub(super) unsafe fn write_at_pinned_segments_with_held_native_mutation(
        &self,
        src: &[PinnedPhysicalSegment],
        offset: u64,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<usize> {
        validate_pinned_physical_segments(src, false)?;
        match self {
            Self::Cached(cached) => unsafe {
                cached.write_at_pinned_segments_with_held_native_mutation(
                    src,
                    offset,
                    false,
                    native_gate,
                )
            },
            Self::Direct(loc) => with_cache_invalidating_file_operation_with_held_native(
                loc,
                native_gate,
                |shared, file| {
                    let _append_guard = shared.append_lock.read();
                    unsafe {
                        write_file_from_pinned_bounce(
                            file,
                            src,
                            offset,
                            validate_pinned_physical_segments(src, false)?,
                        )
                    }
                },
            ),
        }
    }

    /// Nonblocking page-cache admission.  Direct backends deliberately never
    /// claim NOWAIT readiness because they would need lower-device I/O.
    pub fn nowait_range_resident(&self, offset: u64, length: usize) -> bool {
        match self {
            Self::Cached(cache) => cache.nowait_range_resident(offset, length),
            Self::Direct(_) => false,
        }
    }
    /// Returns the page-cache snapshot for an inclusive page interval.
    /// Direct handles intentionally have no resident high-level cache.
    pub fn cachestat(&self, first_page: u64, last_page: u64) -> CachedFileCacheStat {
        match self {
            Self::Cached(cached) => cached.cachestat(first_page, last_page),
            Self::Direct(location) => cached_file_shared_for_location(location)
                .map(|shared| shared.cachestat(first_page, last_page))
                .unwrap_or_default(),
        }
    }

    pub(super) const DIRECT_IO_CHUNK: usize = ALIGNED_BYPASS_CHUNK;

    pub(crate) fn new_direct(location: Location) -> Self {
        Self::Direct(location)
    }

    pub(crate) fn new_cached(location: Location) -> Self {
        Self::Cached(CachedFile::get_or_create(location))
    }

    /// Clones this backend while selecting whether I/O bypasses the page cache.
    ///
    /// Both modes retain the same file location. Direct operations synchronize
    /// and invalidate cached pages before accessing the VFS node.
    pub fn with_direct_io(&self, enabled: bool) -> Self {
        let location = self.location().clone();
        if enabled {
            Self::new_direct(location)
        } else {
            Self::new_cached(location)
        }
    }

    /// Reads data from the file at `offset` into `dst`.
    pub fn read_at(&self, mut dst: impl Write + IoBufMut, mut offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.read_at(dst, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let mut total = 0;
                let mut chunk = vec![0_u8; Self::DIRECT_IO_CHUNK];

                while dst.remaining_mut() > 0 {
                    let limit = dst.remaining_mut().min(chunk.len());
                    let read = match file.read_at(&mut chunk[..limit], offset) {
                        Ok(read) => read,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
                    crate::account_backing_read(read);
                    if read == 0 {
                        break;
                    }
                    let written = match dst.write(&chunk[..read]) {
                        Ok(written) => written,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
                    if written == 0 {
                        break;
                    }
                    offset += written as u64;
                    total += written;
                    if written < read || read < limit {
                        break;
                    }
                }

                Ok(total)
            }),
        }
    }

    pub fn read_at_slice(&self, dst: &mut [u8], offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.read_at_slice(dst, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let async_read = {
                    let mut bufs = [&mut *dst];
                    file.try_read_at_vectored_async(&mut bufs, offset)?
                };
                let read = match async_read {
                    Some(read) => read,
                    None => file.read_at(dst, offset)?,
                };
                crate::account_backing_read(read);
                Ok(read)
            }),
        }
    }

    pub fn read_at_vectored(&self, dst: &mut [&mut [u8]], offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.read_at_vectored(dst, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let read = match file.try_read_at_vectored_async(dst, offset)? {
                    Some(read) => read,
                    None => file.read_at_vectored(dst, offset)?,
                };
                crate::account_backing_read(read);
                Ok(read)
            }),
        }
    }

    /// Prepares an owned ext4 physical effect.  Filesystem mapping admission
    /// runs before cache staging, so an unsupported/hole/unwritten/EOF or
    /// non-regular request returns without device or cache side effects.
    #[cfg(feature = "ext4")]
    pub(crate) fn prepare_physical_io_effect(
        &self,
        operation: PhysicalIoOperation,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<PhysicalIoEffect>> {
        let total = validate_physical_io_segments(segments, offset)?;
        let Self::Direct(location) = self else {
            return Ok(None);
        };
        let end = offset
            .checked_add(u64::try_from(total).map_err(|_| VfsError::InvalidInput)?)
            .ok_or(VfsError::InvalidInput)?;
        let file = location.entry().as_file()?;
        let inode = match file.downcast_owned::<crate::fs::ext4::Inode>() {
            Ok(inode) => inode,
            Err(_) => return Ok(None),
        };
        let Some(effect) = inode.prepare_owned_physical_effect(operation, segments, offset)? else {
            return Ok(None);
        };
        let native_mutation = if operation == PhysicalIoOperation::Write {
            begin_native_location_mutation(location, false)?
        } else {
            begin_source_location_writeback_mutation(location)?
        };

        // Do not create a cached-file registry entry or a range lease until
        // filesystem admission has succeeded.  The lower planner uses its
        // non-caching mapping view, so rejected eligibility remains
        // side-effect free and can synchronously select the fallback.
        let shared = cached_file_shared_for_location_or_create(location);
        let range_lease = match CachedFileShared::try_range_cache_lease(
            &shared,
            offset..end,
            if operation == PhysicalIoOperation::Write {
                RangeCacheLeaseKind::DirectWrite
            } else {
                RangeCacheLeaseKind::DirectRead
            },
        ) {
            Ok(lease) => lease,
            Err(VfsError::ResourceBusy | VfsError::Unsupported) => return Ok(None),
            Err(error) => return Err(error),
        };

        // Only the short cache staging window holds the direct/writeback
        // locks.  The returned transaction and range lease are owned by the
        // effect and survive device submission/completion waits.
        let invalidation_result = (|| -> VfsResult<CachedPageInvalidationTransaction> {
            let _direct_guard = shared.direct_io_lock.lock();
            let _writeback_guard = shared.writeback_lock.write();
            wait_for_all_writeback_clear(&shared);
            let first_page = offset / PAGE_SIZE as u64;
            let last_page = end.div_ceil(PAGE_SIZE as u64);
            let first_page = u32::try_from(first_page).map_err(|_| VfsError::InvalidInput)?;
            let last_page = u32::try_from(last_page).map_err(|_| VfsError::InvalidInput)?;
            let mut invalidation = CachedPageInvalidationTransaction::new_shared(shared.clone());
            invalidation.stage_range(first_page..last_page)?;
            invalidation.prepare_evictions()?;
            invalidation.writeback_with_held_native_gate(
                file,
                true,
                held_native_writeback_gate(&native_mutation),
            )?;
            Ok(invalidation)
        })();
        let invalidation = match invalidation_result {
            Ok(invalidation) => invalidation,
            Err(VfsError::ResourceBusy | VfsError::Unsupported) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(PhysicalIoEffect::new(
            location.clone(),
            inode,
            effect,
            range_lease,
            invalidation,
            if operation == PhysicalIoOperation::Write {
                native_mutation
            } else {
                None
            },
        )))
    }

    /// Attempts direct I/O into caller-pinned physical SG memory.
    ///
    /// `Ok(None)` is the capability/fallback result.  Validation failures and
    /// lower filesystem errors are returned as errors and are never bounced.
    ///
    /// # Safety
    ///
    /// The caller must keep all physical ranges pinned, DMA-accessible,
    /// writable, and disjoint for the complete call. Concurrent CPU/device
    /// access may race on contents and is the caller's responsibility; this
    /// path does not construct Rust references from physical addresses.
    pub unsafe fn try_read_at_dma_segments_with_reason(
        &self,
        dst: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        let total = validate_physical_io_segments(dst, offset)?;
        let result = match self {
            Self::Cached(_) => {
                PhysicalIoAttempt::NotSubmitted(PhysicalIoAttemptNotSubmittedReason::Extent)
            }
            Self::Direct(loc) => with_direct_range_operation_after_preflight(
                loc,
                offset,
                total,
                RangeCacheLeaseKind::DirectRead,
                |_, file| file.physical_read_eligible(dst, offset),
                |_, file| unsafe { file.try_read_at_physical_with_reason(dst, offset) },
            )?
            .unwrap_or(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoAttemptNotSubmittedReason::Extent,
            )),
        };
        if let PhysicalIoAttempt::Completed(bytes) = result {
            if bytes == 0 || bytes > total {
                return Err(VfsError::Io);
            }
            crate::account_backing_read(bytes);
        }
        Ok(result)
    }

    pub unsafe fn try_read_at_dma_segments(
        &self,
        dst: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        Ok(
            match unsafe { self.try_read_at_dma_segments_with_reason(dst, offset)? } {
                PhysicalIoAttempt::Completed(bytes) => Some(bytes),
                PhysicalIoAttempt::NotSubmitted(_) => None,
            },
        )
    }

    /// Performs positioned I/O into pinned physical destination segments.
    ///
    /// # Safety
    ///
    /// The caller must uphold the pin, mapping, and access contract of
    /// [`CachedFile::read_at_pinned_segments`].
    pub unsafe fn read_at_pinned_segments(
        &self,
        dst: &[PinnedPhysicalSegment],
        offset: u64,
        _try_async: bool,
    ) -> VfsResult<usize> {
        validate_pinned_physical_segments(dst, true)?;
        match self {
            Self::Cached(cached) => unsafe { cached.read_at_pinned_segments(dst, offset, false) },
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| unsafe {
                read_file_into_pinned_bounce(
                    file,
                    dst,
                    offset,
                    validate_pinned_physical_segments(dst, true)?,
                )
            }),
        }
    }

    /// Writes `src` to the file at `offset`.
    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(crate) fn write_at(&self, src: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        self.write_at_with_held_native_mutation(
            src,
            offset,
            held_native_writeback_gate(&native_mutation),
        )
    }

    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(super) fn write_at_unchecked(
        &self,
        mut src: impl Read + IoBuf,
        mut offset: u64,
    ) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.write_at(src, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let mut total = 0;
                let mut chunk = vec![0_u8; Self::DIRECT_IO_CHUNK];

                while src.remaining() > 0 {
                    let limit = src.remaining().min(chunk.len());
                    let read = match src.read(&mut chunk[..limit]) {
                        Ok(read) => read,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
                    if read == 0 {
                        break;
                    }
                    let written = match file.write_at(&chunk[..read], offset) {
                        Ok(written) => written,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
                    crate::account_backing_write(written);
                    if written == 0 {
                        break;
                    }
                    offset += written as u64;
                    total += written;
                    if written < read {
                        break;
                    }
                }

                Ok(total)
            }),
        }
    }

    pub(super) fn write_at_with_held_native_mutation(
        &self,
        mut src: impl Read + IoBuf,
        mut offset: u64,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => {
                cached.write_at_with_held_native_mutation(src, offset, native_gate)
            }
            Self::Direct(loc) => with_cache_invalidating_file_operation_with_held_native(
                loc,
                native_gate,
                |_, file| {
                    let mut total = 0;
                    let mut chunk = vec![0_u8; Self::DIRECT_IO_CHUNK];
                    while src.remaining() > 0 {
                        let limit = src.remaining().min(chunk.len());
                        let read = match src.read(&mut chunk[..limit]) {
                            Ok(read) => read,
                            Err(_) if total != 0 => break,
                            Err(error) => return Err(error),
                        };
                        if read == 0 {
                            break;
                        }
                        let written = match file.write_at(&chunk[..read], offset) {
                            Ok(written) => written,
                            Err(_) if total != 0 => break,
                            Err(error) => return Err(error),
                        };
                        crate::account_backing_write(written);
                        if written == 0 {
                            break;
                        }
                        offset += written as u64;
                        total += written;
                        if written < read {
                            break;
                        }
                    }
                    Ok(total)
                },
            ),
        }
    }

    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(crate) fn write_at_slice(&self, src: &[u8], offset: u64) -> VfsResult<usize> {
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        self.write_at_slice_with_held_native_mutation(
            src,
            offset,
            held_native_writeback_gate(&native_mutation),
        )
    }

    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(super) fn write_at_slice_unchecked(&self, src: &[u8], offset: u64) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.write_at_slice(src, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let written = file.write_at(src, offset)?;
                crate::account_backing_write(written);
                Ok(written)
            }),
        }
    }

    pub(super) fn write_at_slice_with_held_native_mutation(
        &self,
        src: &[u8],
        offset: u64,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => {
                cached.write_at_with_held_native_mutation(src, offset, native_gate)
            }
            Self::Direct(loc) => with_cache_invalidating_file_operation_with_held_native(
                loc,
                native_gate,
                |_, file| {
                    let written = file.write_at(src, offset)?;
                    crate::account_backing_write(written);
                    Ok(written)
                },
            ),
        }
    }

    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(crate) fn write_at_vectored(&self, src: &[&[u8]], offset: u64) -> VfsResult<usize> {
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        self.write_at_vectored_with_held_native_mutation(
            src,
            offset,
            held_native_writeback_gate(&native_mutation),
        )
    }

    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(super) fn write_at_vectored_unchecked(
        &self,
        src: &[&[u8]],
        offset: u64,
    ) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => cached.write_at_vectored(src, offset),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |_, file| {
                let written = match file.try_write_at_vectored_async(src, offset)? {
                    AsyncVectoredWriteOutcome::Completed(written) => written,
                    AsyncVectoredWriteOutcome::NotSubmitted => {
                        file.write_at_vectored(src, offset)?
                    }
                    AsyncVectoredWriteOutcome::CompletionError(error) => return Err(error),
                };
                crate::account_backing_write(written);
                Ok(written)
            }),
        }
    }

    pub(super) fn write_at_vectored_with_held_native_mutation(
        &self,
        src: &[&[u8]],
        offset: u64,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<usize> {
        match self {
            Self::Cached(cached) => {
                let mut total = 0usize;
                let mut current = offset;
                for part in src.iter().copied() {
                    if part.is_empty() {
                        continue;
                    }
                    let written =
                        match cached.write_at_with_held_native_mutation(part, current, native_gate)
                        {
                            Ok(written) => written,
                            Err(_) if total != 0 => break,
                            Err(error) => return Err(error),
                        };
                    total += written;
                    current = current
                        .checked_add(written as u64)
                        .ok_or(VfsError::InvalidInput)?;
                    if written < part.len() || written == 0 {
                        break;
                    }
                }
                Ok(total)
            }
            Self::Direct(loc) => with_cache_invalidating_file_operation_with_held_native(
                loc,
                native_gate,
                |_, file| {
                    let written = match file.try_write_at_vectored_async(src, offset)? {
                        AsyncVectoredWriteOutcome::Completed(written) => written,
                        AsyncVectoredWriteOutcome::NotSubmitted => {
                            file.write_at_vectored(src, offset)?
                        }
                        AsyncVectoredWriteOutcome::CompletionError(error) => return Err(error),
                    };
                    crate::account_backing_write(written);
                    Ok(written)
                },
            ),
        }
    }

    /// Attempts direct overwrite I/O from caller-pinned physical SG memory.
    /// The request is never allowed to extend the file.
    ///
    /// # Safety
    ///
    /// The caller must keep all physical ranges pinned, DMA-accessible,
    /// readable, and disjoint for the complete call. Concurrent CPU/device
    /// access may race on contents and is the caller's responsibility; this
    /// path does not construct Rust references from physical addresses.
    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(crate) unsafe fn try_write_at_dma_segments_with_reason(
        &self,
        src: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        unsafe {
            self.try_write_at_dma_segments_with_reason_with_held_native_mutation(
                src,
                offset,
                held_native_writeback_gate(&native_mutation),
            )
        }
    }

    pub(super) unsafe fn try_write_at_dma_segments_with_reason_with_held_native_mutation(
        &self,
        src: &[PhysicalIoSegment],
        offset: u64,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<PhysicalIoAttempt> {
        let total = validate_physical_io_segments(src, offset)?;
        let result = match self {
            Self::Cached(_) => {
                PhysicalIoAttempt::NotSubmitted(PhysicalIoAttemptNotSubmittedReason::Extent)
            }
            Self::Direct(loc) => with_direct_range_operation_after_preflight_with_held_native(
                loc,
                offset,
                total,
                RangeCacheLeaseKind::DirectWrite,
                native_gate,
                |_, file| file.physical_write_eligible(src, offset),
                |_, file| unsafe { file.try_write_at_physical_with_reason(src, offset) },
            )?
            .unwrap_or(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoAttemptNotSubmittedReason::Extent,
            )),
        };
        if let PhysicalIoAttempt::Completed(bytes) = result {
            if bytes != total {
                return Err(VfsError::Io);
            }
            crate::account_backing_write(bytes);
        }
        Ok(result)
    }

    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(crate) unsafe fn try_write_at_dma_segments(
        &self,
        src: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        Ok(
            match unsafe { self.try_write_at_dma_segments_with_reason(src, offset)? } {
                PhysicalIoAttempt::Completed(bytes) => Some(bytes),
                PhysicalIoAttempt::NotSubmitted(_) => None,
            },
        )
    }

    /// Performs positioned I/O from pinned physical source segments.
    ///
    /// # Safety
    ///
    /// The caller must uphold the pin and access contract of
    /// [`CachedFile::write_at_pinned_segments`].
    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(crate) unsafe fn write_at_pinned_segments(
        &self,
        src: &[PinnedPhysicalSegment],
        offset: u64,
        _try_async: bool,
    ) -> VfsResult<usize> {
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        unsafe {
            self.write_at_pinned_segments_with_held_native_mutation(
                src,
                offset,
                held_native_writeback_gate(&native_mutation),
            )
        }
    }

    /// Appends an admitted prefix of `src` under one inode append transaction.
    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(crate) fn append_with_admission(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<(usize, u64)> {
        let native_mutation = begin_native_location_mutation(self.location(), true)?;
        self.append_with_admission_with_held_native_mutation(
            src,
            admit,
            held_native_writeback_gate(&native_mutation),
        )
    }

    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(super) fn append_with_admission_unchecked(
        &self,
        mut src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<(usize, u64)> {
        match self {
            Self::Cached(cached) => cached.append_with_admission(src, admit),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |shared, file| {
                let _append_guard = shared.append_lock.write();
                let mut total = 0;
                let mut end = file.len()?;
                let requested = src.remaining();
                let allowed = admit(end, requested)?;
                if allowed > requested {
                    return Err(VfsError::InvalidInput);
                }
                let mut admitted = (&mut src).take(allowed as u64);
                let mut chunk = vec![0_u8; Self::DIRECT_IO_CHUNK];

                while admitted.remaining() > 0 {
                    let limit = admitted.remaining().min(chunk.len());
                    let read = match admitted.read(&mut chunk[..limit]) {
                        Ok(read) => read,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
                    if read == 0 {
                        break;
                    }
                    let (written, new_end) = match file.append(&chunk[..read]) {
                        Ok(result) => result,
                        Err(_) if total != 0 => break,
                        Err(error) => return Err(error),
                    };
                    if written == 0 {
                        break;
                    }
                    total += written;
                    end = new_end;
                    if written < read {
                        break;
                    }
                }

                Ok((total, end))
            }),
        }
    }

    /// RWF_NOWAIT append admission. Direct handles deliberately decline here
    /// unless they grow an equivalent prepared no-wait invalidation path:
    /// entering the legacy cache-invalidation transaction could sleep on its
    /// direct-I/O/writeback locks after a successful NOWAIT admission.
    pub(crate) fn try_append_with_admission(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<Option<(usize, u64)>> {
        let _native_mutation = try_begin_native_location_mutation_nowait(self.location(), true)?;
        match self {
            Self::Cached(cached) => cached.try_append_with_admission(src, admit),
            Self::Direct(_) => Ok(None),
        }
    }

    /// Appends `src` to the end of the file. Returns `(bytes_written, new_end)`.
    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(crate) fn append(&self, src: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        self.append_with_admission(src, |_offset, requested| Ok(requested))
    }

    /// Appends one scatter list without releasing the inode append domain
    /// between nonempty elements.
    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(crate) fn append_vectored(&self, src: &[&[u8]]) -> VfsResult<(usize, u64)> {
        let native_mutation = begin_native_location_mutation(self.location(), true)?;
        self.append_vectored_with_held_native_mutation(
            src,
            held_native_writeback_gate(&native_mutation),
        )
    }

    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(super) fn append_vectored_unchecked(&self, src: &[&[u8]]) -> VfsResult<(usize, u64)> {
        match self {
            Self::Cached(cached) => cached.append_vectored(src),
            Self::Direct(loc) => with_cache_invalidating_file_operation(loc, |shared, file| {
                let _append_guard = shared.append_lock.write();
                let mut total = 0usize;
                let mut end = file.len()?;

                // The lower async vectored API is positioned I/O, not an
                // append operation. Keep using FileNodeOps::append so a
                // filesystem's own EOF/inode rules remain in force; the
                // shared high-level guards make all chunks and iovecs one
                // transaction relative to every other axfs writer.
                for buf in src.iter().copied() {
                    if buf.is_empty() {
                        continue;
                    }
                    let requested = buf.len();
                    let mut element_written = 0usize;
                    while element_written < requested {
                        let limit = (requested - element_written).min(Self::DIRECT_IO_CHUNK);
                        let chunk = &buf[element_written..element_written + limit];
                        let (written, new_end) = match file.append(chunk) {
                            Ok(result) => result,
                            Err(_) if total != 0 => break,
                            Err(error) => return Err(error),
                        };
                        total += written;
                        element_written += written;
                        end = new_end;
                        if written < limit || written == 0 {
                            break;
                        }
                    }
                    if element_written < requested || element_written == 0 {
                        break;
                    }
                }

                Ok((total, end))
            }),
        }
    }

    /// Returns a reference to the underlying [`Location`].
    pub fn location(&self) -> &Location {
        match self {
            Self::Cached(cached) => cached.location(),
            Self::Direct(loc) => loc,
        }
    }

    pub(super) fn fadvise_cache(&self) -> Option<CachedFile> {
        match self {
            Self::Cached(cache) => Some(cache.clone()),
            // An O_DIRECT description has no private buffered cache, but the
            // inode can still have one through another OFD/mapping. Advice
            // must never instantiate that cache through an infallible Arc or
            // registry allocation, so a cache-less direct inode is a no-op.
            Self::Direct(location) => CachedFile::get_existing(location.clone()),
        }
    }

    pub fn fadvise_willneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        match self.fadvise_cache() {
            Some(cache) => cache.fadvise_willneed(offset, len),
            None => Ok(()),
        }
    }

    pub fn fadvise_noreuse(&self, offset: u64, len: u64) -> VfsResult<()> {
        match self.fadvise_cache() {
            Some(cache) => cache.fadvise_noreuse(offset, len),
            None => Ok(()),
        }
    }

    pub fn fadvise_dontneed(&self, offset: u64, len: u64) -> VfsResult<()> {
        match self.fadvise_cache() {
            Some(cache) => cache.fadvise_dontneed(offset, len),
            None => Ok(()),
        }
    }

    pub fn read_at_with_readahead(
        &self,
        dst: impl Write + IoBufMut,
        offset: u64,
        readahead: bool,
    ) -> VfsResult<usize> {
        match self {
            Self::Cached(cache) => cache.read_at_with_readahead(dst, offset, readahead),
            Self::Direct(_) => self.read_at(dst, offset),
        }
    }

    /// Flushes cached data (and optionally metadata) to disk.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        record_file_sync_request(data_only);
        match self {
            Self::Cached(cached) => cached.sync(data_only),
            Self::Direct(loc) => {
                with_cache_invalidating_file_operation(loc, |_, file| file.sync(data_only))
            }
        }
    }

    pub fn sync_range(&self, offset: u64, len: u64, data_only: bool) -> VfsResult<()> {
        match self {
            Self::Cached(cached) => cached.sync_range(offset, len, data_only),
            Self::Direct(loc) => {
                with_cache_invalidating_file_operation(loc, |_, file| file.sync(data_only))
            }
        }
    }

    pub fn range_writeback_snapshot(&self) -> RangeWritebackFence {
        match self {
            Self::Cached(cached) => cached.range_writeback_snapshot(),
            Self::Direct(_) => RangeWritebackFence::none(),
        }
    }

    pub fn submit_range_writeback(
        &self,
        offset: u64,
        len: u64,
        data_only: bool,
    ) -> VfsResult<RangeWritebackFence> {
        match self {
            Self::Cached(cached) => cached.submit_range_writeback(offset, len, data_only),
            // There is no page-cache writeback domain for direct handles.
            // Do not substitute a whole-file sync; Linux treats this as an
            // implementation-supported no-op when there is nothing queued.
            Self::Direct(_) => Ok(RangeWritebackFence::none()),
        }
    }

    pub fn wait_range_writeback_through(
        &self,
        fence: &RangeWritebackFence,
        offset: u64,
        len: u64,
    ) -> VfsResult<()> {
        match self {
            Self::Cached(cached) => cached.wait_range_writeback_through(fence, offset, len),
            Self::Direct(_) => Ok(()),
        }
    }

    /// Truncates or extends the file to `len` bytes.
    // Backend write API for the in-progress write path.
    #[allow(dead_code)]
    pub(crate) fn set_len(&self, len: u64) -> VfsResult<()> {
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        self.set_len_with_held_native_mutation(len, held_native_writeback_gate(&native_mutation))
    }

    pub(super) fn set_len_with_held_native_mutation(
        &self,
        len: u64,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<()> {
        match self {
            Self::Cached(cached) => cached.set_len_with_held_native_mutation(len, native_gate),
            Self::Direct(loc) => {
                with_cache_invalidating_truncate_with_held_native(loc, len, native_gate)
            }
        }
    }
}
