//! File: positioned reads, writes and appends.

use super::*;

impl File {
    /// Reads a number of bytes starting from a given offset.
    pub fn read_at(&self, dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        #[cfg(feature = "times")]
        let requested = dst.remaining_mut();
        self.access(FileFlags::READ)?;
        let read = if let Some(handle) = &self.open_handle {
            let mut dst = dst;
            let mut offset = offset;
            let mut total = 0;
            let mut scratch = [0_u8; 8192];
            while dst.remaining_mut() != 0 {
                let limit = dst.remaining_mut().min(scratch.len());
                let read = match handle.read_at(&mut scratch[..limit], offset) {
                    Ok(read) => read,
                    Err(_) if total != 0 => break,
                    Err(error) => return Err(error),
                };
                if read == 0 {
                    break;
                }
                let written = dst.write(&scratch[..read])?;
                total += written;
                offset = offset
                    .checked_add(written as u64)
                    .ok_or(VfsError::InvalidInput)?;
                if written != read || read != limit {
                    break;
                }
            }
            total
        } else {
            self.inner
                .read_at_with_readahead(dst, offset, !self.fadvise_has(FADVISE_RANDOM))?
        };
        if read != 0 && self.fadvise_has(FADVISE_NOREUSE) {
            // Persist across future reads of this OFD, while marking only
            // pages actually consumed by this read.
            let _ = self.inner.fadvise_noreuse(offset, read as u64);
        }
        if read != 0 && self.fadvise_has(FADVISE_SEQUENTIAL) && !self.fadvise_has(FADVISE_RANDOM) {
            let next = offset.saturating_add(read as u64);
            let _ = self
                .inner
                .fadvise_willneed(next, (READAHEAD_PAGES * 2 * PAGE_SIZE) as u64);
        }
        #[cfg(feature = "times")]
        if requested > 0
            && !self.flags.contains(FileFlags::NOATIME)
            && FsContext::should_update_atime(self.location())
        {
            self.record_time_flags(1);
            self.flush_times();
        }
        Ok(read)
    }

    /// Attempts a positioned direct read into caller-pinned physical memory.
    /// `Ok(None)` permits the caller to use its ordinary fallback path.
    ///
    /// # Safety
    ///
    /// All segments must remain pinned, DMA-accessible, writable, and disjoint
    /// until the call returns. Concurrent CPU/device content races are the
    /// caller's responsibility and do not make physical addresses into Rust
    /// references.
    pub unsafe fn try_read_at_dma_segments_with_reason(
        &self,
        dst: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        if !self.supports_positioned_read() {
            return Ok(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoAttemptNotSubmittedReason::Extent,
            ));
        }
        let result = unsafe {
            self.access(FileFlags::READ)?
                .try_read_at_dma_segments_with_reason(dst, offset)
        }?;
        #[cfg(feature = "times")]
        if matches!(result, PhysicalIoAttempt::Completed(bytes) if bytes != 0)
            && !self.flags.contains(FileFlags::NOATIME)
            && FsContext::should_update_atime(self.location())
        {
            self.record_time_flags(1);
            self.flush_times();
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

    pub fn read_at_slice(&self, dst: &mut [u8], offset: u64) -> VfsResult<usize> {
        #[cfg(feature = "times")]
        let requested = dst.len();
        self.access(FileFlags::READ)?;
        let read = if let Some(handle) = &self.open_handle {
            handle.read_at(dst, offset)?
        } else {
            self.inner.read_at_slice(dst, offset)?
        };
        #[cfg(feature = "times")]
        if requested > 0
            && !self.flags.contains(FileFlags::NOATIME)
            && FsContext::should_update_atime(self.location())
        {
            self.record_time_flags(1);
            self.flush_times();
        }
        Ok(read)
    }

    pub fn read_at_vectored_slice(&self, dst: &mut [&mut [u8]], offset: u64) -> VfsResult<usize> {
        #[cfg(feature = "times")]
        let requested = dst.iter().map(|buf| buf.len()).sum::<usize>();
        self.access(FileFlags::READ)?;
        let read = if let Some(handle) = &self.open_handle {
            handle.read_at_vectored(dst, offset)?
        } else {
            self.inner.read_at_vectored(dst, offset)?
        };
        #[cfg(feature = "times")]
        if requested > 0
            && !self.flags.contains(FileFlags::NOATIME)
            && FsContext::should_update_atime(self.location())
        {
            self.record_time_flags(1);
            self.flush_times();
        }
        Ok(read)
    }

    /// Reads at `offset` into caller-pinned physical segments.
    ///
    /// # Safety
    ///
    /// The caller must keep every destination pinned, mapped, writable, and
    /// accessible through the call. Ranges must be mutually disjoint, but may
    /// remain concurrently accessible to userspace because no Rust reference
    /// is created for them.
    pub unsafe fn read_at_pinned_segments(
        &self,
        dst: &[PinnedPhysicalSegment],
        offset: u64,
        try_async: bool,
    ) -> VfsResult<usize> {
        #[cfg(feature = "times")]
        let requested = validate_pinned_physical_segments(dst, true)?;
        let read = unsafe {
            self.access(FileFlags::READ)?
                .read_at_pinned_segments(dst, offset, try_async)?
        };
        #[cfg(feature = "times")]
        if requested > 0
            && !self.flags.contains(FileFlags::NOATIME)
            && FsContext::should_update_atime(self.location())
        {
            self.record_time_flags(1);
            self.flush_times();
        }
        Ok(read)
    }

    /// Writes a number of bytes starting from a given offset.
    pub fn write_at(&self, src: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        self.admit_native_mutation(Some(offset), false)?;
        self.access(FileFlags::WRITE)?;
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        let written = if let Some(handle) = &self.open_handle {
            let mut src = src;
            let mut offset = offset;
            let mut total = 0;
            let mut scratch = [0_u8; 8192];
            while src.remaining() != 0 {
                let limit = src.remaining().min(scratch.len());
                src.read(&mut scratch[..limit])?;
                let written = match handle.write_at(&scratch[..limit], offset) {
                    Ok(written) => written,
                    Err(_) if total != 0 => break,
                    Err(error) => return Err(error),
                };
                total += written;
                offset = offset
                    .checked_add(written as u64)
                    .ok_or(VfsError::InvalidInput)?;
                if written != limit {
                    break;
                }
            }
            total
        } else {
            self.inner.write_at_with_held_native_mutation(
                src,
                offset,
                held_native_writeback_gate(&native_mutation),
            )?
        };
        #[cfg(feature = "times")]
        if written > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(written)
    }

    pub fn write_at_slice(&self, src: &[u8], offset: u64) -> VfsResult<usize> {
        self.admit_native_mutation(Some(offset), false)?;
        self.access(FileFlags::WRITE)?;
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        let written = if let Some(handle) = &self.open_handle {
            handle.write_at(src, offset)?
        } else {
            self.inner.write_at_slice_with_held_native_mutation(
                src,
                offset,
                held_native_writeback_gate(&native_mutation),
            )?
        };
        #[cfg(feature = "times")]
        if written > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(written)
    }

    pub fn write_at_vectored_slice(&self, src: &[&[u8]], offset: u64) -> VfsResult<usize> {
        self.admit_native_mutation(Some(offset), false)?;
        self.access(FileFlags::WRITE)?;
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        let written = if let Some(handle) = &self.open_handle {
            let mut total = 0;
            let mut cursor = offset;
            for part in src {
                if part.is_empty() {
                    continue;
                }
                let written = match handle.write_at(part, cursor) {
                    Ok(written) => written,
                    Err(_) if total != 0 => break,
                    Err(error) => return Err(error),
                };
                total += written;
                cursor = cursor
                    .checked_add(written as u64)
                    .ok_or(VfsError::InvalidInput)?;
                if written != part.len() {
                    break;
                }
            }
            total
        } else {
            self.inner.write_at_vectored_with_held_native_mutation(
                src,
                offset,
                held_native_writeback_gate(&native_mutation),
            )?
        };
        #[cfg(feature = "times")]
        if written > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(written)
    }

    /// Writes at `offset` from caller-pinned physical segments.
    ///
    /// # Safety
    ///
    /// The caller must keep every source pinned, mapped, readable, and
    /// accessible through the call. Concurrent userspace access is permitted;
    /// the implementation creates no Rust reference to the physical ranges.
    pub unsafe fn write_at_pinned_segments(
        &self,
        src: &[PinnedPhysicalSegment],
        offset: u64,
        _try_async: bool,
    ) -> VfsResult<usize> {
        self.admit_native_mutation(Some(offset), false)?;
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        let written = unsafe {
            self.access(FileFlags::WRITE)?
                .write_at_pinned_segments_with_held_native_mutation(
                    src,
                    offset,
                    held_native_writeback_gate(&native_mutation),
                )?
        };
        #[cfg(feature = "times")]
        if written > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(written)
    }

    /// Attempts a positioned direct overwrite from caller-pinned physical
    /// memory. O_APPEND and nodes without positioned writes intentionally
    /// return `Ok(None)` without entering the backend.
    ///
    /// # Safety
    ///
    /// All segments must remain pinned, DMA-accessible, readable, and disjoint
    /// until the call returns. Concurrent CPU/device content races are the
    /// caller's responsibility and do not make physical addresses into Rust
    /// references.
    pub unsafe fn try_write_at_dma_segments_with_reason(
        &self,
        src: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        self.admit_native_mutation(Some(offset), false)?;
        if self.append_enabled() || !self.supports_positioned_write() {
            return Ok(PhysicalIoAttempt::NotSubmitted(
                PhysicalIoAttemptNotSubmittedReason::DeviceAdmission,
            ));
        }
        // The lower DMA operation can mutate storage after range/cache
        // preflight, so retain the stable inode admission through its device
        // completion (this method's contract returns only after completion).
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        let result = unsafe {
            self.access(FileFlags::WRITE)?
                .try_write_at_dma_segments_with_reason_with_held_native_mutation(
                    src,
                    offset,
                    held_native_writeback_gate(&native_mutation),
                )
        }?;
        #[cfg(feature = "times")]
        if matches!(result, PhysicalIoAttempt::Completed(bytes) if bytes != 0) {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(result)
    }

    pub unsafe fn try_write_at_dma_segments(
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

    pub(super) fn write_at_end_with_admission_and_new_end(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<(usize, u64)> {
        self.admit_native_mutation(None, true)?;
        let native_mutation = begin_native_location_mutation(self.location(), true)?;
        let result = self
            .access(FileFlags::WRITE)?
            .append_with_admission_with_held_native_mutation(
                src,
                admit,
                held_native_writeback_gate(&native_mutation),
            )?;
        #[cfg(feature = "times")]
        if result.0 > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(result)
    }

    pub(super) fn write_at_end_with_new_end(
        &self,
        src: impl Read + IoBuf,
    ) -> VfsResult<(usize, u64)> {
        self.write_at_end_with_admission_and_new_end(src, |_offset, requested| Ok(requested))
    }

    pub(super) fn write_vectored_at_end_with_new_end(
        &self,
        src: &[&[u8]],
    ) -> VfsResult<(usize, Option<u64>)> {
        self.admit_native_mutation(None, true)?;
        let backend = self.access(FileFlags::WRITE)?;
        if !src.iter().any(|buf| !buf.is_empty()) {
            return Ok((0, None));
        }
        let native_mutation = begin_native_location_mutation(self.location(), true)?;
        let (total, new_end) = backend.append_vectored_with_held_native_mutation(
            src,
            held_native_writeback_gate(&native_mutation),
        )?;
        #[cfg(feature = "times")]
        if total > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok((total, Some(new_end)))
    }

    /// Atomically appends data without reading or changing this [`File`]'s
    /// current position.
    ///
    /// This is the positioned counterpart of
    /// [`write_with_placement`](Self::write_with_placement) with
    /// [`WritePlacement::End`].
    pub fn write_at_end(&self, src: impl Read + IoBuf) -> VfsResult<usize> {
        self.write_at_end_with_new_end(src)
            .map(|(written, _)| written)
    }

    /// Atomically appends an admitted prefix without changing this [`File`]'s
    /// current position.
    ///
    /// The admission callback observes the exact inode end protected by the
    /// append serialization domain. It must be short and must not call another
    /// append or current-position operation on this file.
    pub fn write_at_end_with_admission(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<usize> {
        self.write_at_end_with_admission_and_new_end(src, admit)
            .map(|(written, _)| written)
    }

    /// Atomically appends an admitted prefix and returns the exact byte range
    /// start chosen by the same append transaction.  Callers that need a
    /// post-I/O cache operation must use this rather than sampling EOF after
    /// the append lock has been released.
    pub fn write_at_end_with_admission_and_start(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<(usize, u64)> {
        self.write_at_end_with_admission_and_new_end(src, admit)
            .map(|(written, end)| (written, end.saturating_sub(written as u64)))
    }

    /// Prepared RWF_NOWAIT positioned-append transaction.  Every shared
    /// append/direct-I/O gate is acquired through the backend try path before
    /// the exact EOF admission callback observes state.
    pub fn try_write_at_end_with_admission_and_start(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<Option<(usize, u64)>> {
        // RWF_NOWAIT append must not take the normal provider getter.  Keep
        // this owned try-only token through append submission and completion.
        let _native_mutation = try_begin_native_location_mutation_nowait(self.location(), true)?;
        let Some((written, end)) = self
            .access(FileFlags::WRITE)?
            .try_append_with_admission(src, admit)?
        else {
            return Ok(None);
        };
        #[cfg(feature = "times")]
        if written > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        Ok(Some((written, end.saturating_sub(written as u64))))
    }

    /// Appends under this open file's current-position transaction and sets
    /// the cursor to the resulting EOF.  This is the `pwritev2(offset=-1)`
    /// append form: unlike positioned append it advances `f_pos`, while the
    /// returned start is still selected by the same append transaction.
    pub fn write_at_current_append_with_admission_and_start(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<(usize, u64)> {
        let _transaction = self.position_transaction.lock();
        let pos = self.position.as_ref().ok_or(VfsError::InvalidInput)?;
        let (written, end) = self.write_at_end_with_admission_and_new_end(src, admit)?;
        if written != 0 {
            *pos.lock() = end;
        }
        Ok((written, end.saturating_sub(written as u64)))
    }

    /// Nonblocking cursor admission for the `pwritev2(offset=-1, O_APPEND)`
    /// form. `None` means another operation owns this OFD cursor and a
    /// RWF_NOWAIT caller must return EAGAIN without entering append work.
    pub fn try_write_at_current_append_with_admission_and_start(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<Option<(usize, u64)>> {
        let Some(_transaction) = self.position_transaction.try_lock() else {
            return Ok(None);
        };
        let pos = self.position.as_ref().ok_or(VfsError::InvalidInput)?;
        // NOWAIT must not consult the blocking fileattr path.
        let _native_mutation = try_begin_native_location_mutation_nowait(self.location(), true)?;
        let Some((written, end)) = self
            .access(FileFlags::WRITE)?
            .try_append_with_admission(src, admit)?
        else {
            return Ok(None);
        };
        #[cfg(feature = "times")]
        if written > 0 {
            self.record_time_flags(2);
            self.flush_times();
        }
        if written != 0 {
            *pos.lock() = end;
        }
        Ok(Some((written, end.saturating_sub(written as u64))))
    }

    /// Atomically appends a byte slice without changing this [`File`]'s
    /// current position.
    pub fn write_at_end_slice(&self, src: &[u8]) -> VfsResult<usize> {
        self.write_at_end_with_new_end(src)
            .map(|(written, _)| written)
    }

    /// Atomically appends a vectored input without changing this [`File`]'s
    /// current position.
    pub fn write_at_end_vectored_slice(&self, src: &[&[u8]]) -> VfsResult<usize> {
        self.write_vectored_at_end_with_new_end(src)
            .map(|(written, _)| written)
    }

    /// Attempts to sync OS-internal file content and metadata to disk.
    ///
    /// If `data_only` is `true`, only the file data is synced, not the
    /// metadata.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        self.access(FileFlags::empty())?;
        // Fold deferred NOWAIT times into the same durability operation. For
        // a direct/OFD-private backend this must happen before its sync so a
        // successful fsync cannot leave freshly written metadata merely
        // provider-visible rather than durable.
        self.location().flush_metadata_time_overlay()?;
        let result = if let Some(handle) = &self.open_handle {
            handle.sync(data_only)
        } else {
            self.inner.sync(data_only)
        };
        result?;
        Ok(())
    }

    pub fn sync_range(&self, offset: u64, len: u64, data_only: bool) -> VfsResult<()> {
        self.access(FileFlags::empty())?;
        self.inner.sync_range(offset, len, data_only)
    }

    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        self.access(FileFlags::WRITE)?;
        self.admit_native_mutation(Some(0), false)?;
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        self.inner
            .set_len_with_held_native_mutation(len, held_native_writeback_gate(&native_mutation))
    }

    /// Commits the one destructive O_TRUNC operation deferred by
    /// `open_loc_deferred_truncate`.  This capability is OFD-private and is
    /// consumed before mutation, so a duplicate/shared description cannot
    /// commit twice and an ordinary read-only `File` cannot manufacture it.
    pub fn commit_deferred_open_truncate(&self) -> VfsResult<()> {
        if self
            .pending_open_truncate
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(VfsError::InvalidInput);
        }
        let native_mutation = begin_native_location_mutation(self.location(), false)?;
        self.inner
            .set_len_with_held_native_mutation(0, held_native_writeback_gate(&native_mutation))
    }

    #[cfg(feature = "ext4")]
    pub fn prepare_physical_io_effect(
        &self,
        operation: PhysicalIoOperation,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<PhysicalIoEffect>> {
        if operation == PhysicalIoOperation::Write && self.append_enabled() {
            return Ok(None);
        }
        match operation {
            PhysicalIoOperation::Read => self.access(FileFlags::READ)?,
            PhysicalIoOperation::Write => self.access(FileFlags::WRITE)?,
        };
        self.inner
            .prepare_physical_io_effect(operation, segments, offset)
    }

    pub fn range_writeback_snapshot(&self) -> VfsResult<RangeWritebackFence> {
        Ok(self.access(FileFlags::empty())?.range_writeback_snapshot())
    }

    pub fn submit_range_writeback(
        &self,
        offset: u64,
        len: u64,
        data_only: bool,
    ) -> VfsResult<RangeWritebackFence> {
        self.access(FileFlags::empty())?
            .submit_range_writeback(offset, len, data_only)
    }

    pub fn wait_range_writeback_through(
        &self,
        fence: &RangeWritebackFence,
        offset: u64,
        len: u64,
    ) -> VfsResult<()> {
        self.access(FileFlags::empty())?
            .wait_range_writeback_through(fence, offset, len)
    }

    /// Returns a typed map of allocated extents for this open file.  Access is
    /// checked before an optional synchronous flush; the flush completes and
    /// releases all cached-file/filesystem locks before the inode query starts.
    pub fn map_extents(
        &self,
        start: u64,
        length: u64,
        max_extents: usize,
        sync: bool,
    ) -> VfsResult<FileExtentMap> {
        self.access(FileFlags::empty())?;
        if sync {
            self.sync(false)?;
        }
        self.location()
            .entry()
            .as_file()?
            .map_extents(start, length, max_extents)
    }

    /// Returns the filesystem's exclusive upper bound for extent queries.
    pub fn max_extent_bytes(&self) -> VfsResult<u64> {
        self.access(FileFlags::empty())?
            .location()
            .entry()
            .as_file()?
            .max_extent_bytes()
    }

    /// Whether the backing filesystem provides allocated extent mappings.
    pub fn supports_extent_mapping(&self) -> VfsResult<bool> {
        Ok(self.location().entry().as_file()?.supports_extent_mapping())
    }
}
