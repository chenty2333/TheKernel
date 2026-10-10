//! CachedFile: aligned bypass I/O and page-cache population.

use super::*;

impl CachedFile {
    pub(super) fn try_read_aligned_bypass(
        &self,
        dst: &mut (impl Write + IoBufMut),
        offset: u64,
        len: usize,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&READ_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&READ_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&READ_BYPASS_ELIGIBLE, 1);

        let _direct_guard = self.shared.direct_io_lock.lock();
        let _mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        if self.range_has_cached_page(&pages) {
            record_cached_file_counter(&READ_BYPASS_REJECT_CACHED, 1);
            return Ok(None);
        }

        let file = self.inner.entry().as_file()?;
        let mut total = 0;
        let mut current = offset;
        let mut chunk = vec![0_u8; ALIGNED_BYPASS_CHUNK.min(len).max(PAGE_SIZE)];
        while total < len && dst.remaining_mut() > 0 {
            let limit = (len - total).min(chunk.len()).min(dst.remaining_mut());
            let async_read = {
                let mut bufs = [&mut chunk[..limit]];
                match file.try_read_at_vectored_async(&mut bufs, current) {
                    Ok(read) => read,
                    Err(_) if total != 0 => break,
                    Err(error) => return Err(error),
                }
            };
            let read = match async_read {
                Some(read) => read,
                None => match file.read_at(&mut chunk[..limit], current) {
                    Ok(read) => read,
                    Err(_) if total != 0 => break,
                    Err(error) => return Err(error),
                },
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
            total += written;
            current += written as u64;
            if written < read || read < limit {
                break;
            }
        }
        if total > 0 {
            record_cached_file_counter(&READ_BYPASS_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_BYTES, total as u64);
        } else {
            record_cached_file_counter(&READ_BYPASS_EOF_RACES, 1);
        }
        Ok(Some(total))
    }

    pub(super) fn try_read_aligned_slice_bypass(
        &self,
        dst: &mut [u8],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&READ_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, dst.len()) else {
            record_cached_file_counter(&READ_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&READ_BYPASS_ELIGIBLE, 1);

        let _direct_guard = self.shared.direct_io_lock.lock();
        let _mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        if self.range_has_cached_page(&pages) {
            record_cached_file_counter(&READ_BYPASS_REJECT_CACHED, 1);
            return Ok(None);
        }

        let file = self.inner.entry().as_file()?;
        let async_read = {
            let mut bufs = [&mut *dst];
            file.try_read_at_vectored_async(&mut bufs, offset)?
        };
        let read = match async_read {
            Some(read) => read,
            None => file.read_at(dst, offset)?,
        };
        crate::account_backing_read(read);
        if read > 0 {
            record_cached_file_counter(&READ_BYPASS_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_BYTES, read as u64);
            record_cached_file_counter(&READ_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_SLICE_BYTES, read as u64);
        } else {
            record_cached_file_counter(&READ_BYPASS_EOF_RACES, 1);
        }
        Ok(Some(read))
    }

    pub(super) fn try_read_aligned_vectored_bypass(
        &self,
        dst: &mut [&mut [u8]],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&READ_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let len = dst.iter().map(|buf| buf.len()).sum();
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&READ_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&READ_BYPASS_ELIGIBLE, 1);

        let _direct_guard = self.shared.direct_io_lock.lock();
        let _mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        if self.range_has_cached_page(&pages) {
            record_cached_file_counter(&READ_BYPASS_REJECT_CACHED, 1);
            return Ok(None);
        }

        let file = self.inner.entry().as_file()?;
        let read = match file.try_read_at_vectored_async(dst, offset)? {
            Some(read) => read,
            None => file.read_at_vectored(dst, offset)?,
        };
        crate::account_backing_read(read);
        if read > 0 {
            record_cached_file_counter(&READ_BYPASS_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_BYTES, read as u64);
            record_cached_file_counter(&READ_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_SLICE_BYTES, read as u64);
        } else {
            record_cached_file_counter(&READ_BYPASS_EOF_RACES, 1);
        }
        Ok(Some(read))
    }

    pub(super) fn try_read_aligned_pinned_bypass(
        &self,
        dst: &[PinnedPhysicalSegment],
        offset: u64,
        len: usize,
        _try_async: bool,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&READ_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&READ_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&READ_BYPASS_ELIGIBLE, 1);

        // Cache invalidation and pin admission must exclude cached aliases
        // before the raw copy into caller-pinned memory starts.
        let _direct_guard = self.shared.direct_io_lock.lock();
        let _mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        if self.range_has_cached_page(&pages) {
            record_cached_file_counter(&READ_BYPASS_REJECT_CACHED, 1);
            return Ok(None);
        }

        let file = self.inner.entry().as_file()?;
        // Pinned user pages remain concurrently accessible from userspace.
        // Keep Rust references confined to kernel-owned bounce storage.
        let read = unsafe { read_file_into_pinned_bounce(file, dst, offset, len) }?;
        if read > 0 {
            record_cached_file_counter(&READ_BYPASS_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_BYTES, read as u64);
            record_cached_file_counter(&READ_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&READ_BYPASS_SLICE_BYTES, read as u64);
        } else {
            record_cached_file_counter(&READ_BYPASS_EOF_RACES, 1);
        }
        Ok(Some(read))
    }

    pub(super) fn try_write_aligned_bypass(
        &self,
        src: &mut (impl Read + IoBuf),
        offset: u64,
        len: usize,
    ) -> VfsResult<Option<usize>> {
        let native_mutation = begin_source_location_writeback_mutation(&self.inner)?;
        self.try_write_aligned_bypass_with_held_native_mutation(
            src,
            offset,
            len,
            held_native_writeback_gate(&native_mutation),
        )
    }

    pub(super) fn try_write_aligned_bypass_with_held_native_mutation(
        &self,
        src: &mut (impl Read + IoBuf),
        offset: u64,
        len: usize,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&WRITE_BYPASS_ELIGIBLE, 1);

        let _direct_guard = self.shared.direct_io_lock.lock();
        let _writeback_guard = self.shared.writeback_lock.write();
        let mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        match self.invalidate_cached_range_with_held_native_gate(
            file,
            pages.clone(),
            &mutation,
            native_gate,
        ) {
            Ok(_) => {}
            // A source backed by one of the target cache pages carries a
            // precise page pin. Keep that page resident and let the cached
            // overlap-aware path below move it through a bounce buffer.
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        }

        let mut total = 0;
        let mut current = offset;
        let mut chunk = vec![0_u8; ALIGNED_BYPASS_CHUNK.min(len).max(PAGE_SIZE)];
        while total < len && src.remaining() > 0 {
            let limit = (len - total).min(chunk.len()).min(src.remaining());
            let read = match src.read(&mut chunk[..limit]) {
                Ok(read) => read,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            if read == 0 {
                break;
            }
            let written = match file.write_at(&chunk[..read], current) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            crate::account_backing_write(written);
            if written == 0 {
                break;
            }
            total += written;
            current += written as u64;
            if written < read {
                break;
            }
        }

        self.invalidate_cached_range_with_held_native_gate(file, pages, &mutation, native_gate)?;
        if total > 0 {
            record_cached_file_counter(&WRITE_BYPASS_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_BYTES, total as u64);
        }
        Ok(Some(total))
    }

    pub(super) fn try_write_aligned_slice_bypass(
        &self,
        src: &[u8],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, src.len()) else {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&WRITE_BYPASS_ELIGIBLE, 1);

        let native_mutation = begin_source_location_writeback_mutation(&self.inner)?;
        let _direct_guard = self.shared.direct_io_lock.lock();
        let _writeback_guard = self.shared.writeback_lock.write();
        let mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        self.invalidate_cached_range_with_held_native_gate(
            file,
            pages.clone(),
            &mutation,
            held_native_writeback_gate(&native_mutation),
        )?;

        let written = file.write_at(src, offset)?;
        crate::account_backing_write(written);

        self.invalidate_cached_range_with_held_native_gate(
            file,
            pages,
            &mutation,
            held_native_writeback_gate(&native_mutation),
        )?;
        if written > 0 {
            record_cached_file_counter(&WRITE_BYPASS_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_BYTES, written as u64);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_BYTES, written as u64);
        }
        Ok(Some(written))
    }

    pub(super) fn try_write_aligned_vectored_bypass(
        &self,
        src: &[&[u8]],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let len = src.iter().map(|buf| buf.len()).sum();
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&WRITE_BYPASS_ELIGIBLE, 1);

        let native_mutation = begin_source_location_writeback_mutation(&self.inner)?;
        let _direct_guard = self.shared.direct_io_lock.lock();
        let _writeback_guard = self.shared.writeback_lock.write();
        let mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        self.invalidate_cached_range_with_held_native_gate(
            file,
            pages.clone(),
            &mutation,
            held_native_writeback_gate(&native_mutation),
        )?;

        let written = match file.try_write_at_vectored_async(src, offset)? {
            AsyncVectoredWriteOutcome::Completed(written) => written,
            AsyncVectoredWriteOutcome::NotSubmitted => file.write_at_vectored(src, offset)?,
            AsyncVectoredWriteOutcome::CompletionError(error) => return Err(error),
        };
        crate::account_backing_write(written);

        self.invalidate_cached_range_with_held_native_gate(
            file,
            pages,
            &mutation,
            held_native_writeback_gate(&native_mutation),
        )?;
        if written > 0 {
            record_cached_file_counter(&WRITE_BYPASS_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_BYTES, written as u64);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_BYTES, written as u64);
        }
        Ok(Some(written))
    }

    // Direct-I/O pinned bypass for the in-progress write path.
    #[allow(dead_code)]
    pub(super) fn try_write_aligned_pinned_bypass(
        &self,
        src: &[PinnedPhysicalSegment],
        offset: u64,
        len: usize,
        try_async: bool,
    ) -> VfsResult<Option<usize>> {
        let native_mutation = begin_source_location_writeback_mutation(&self.inner)?;
        self.try_write_aligned_pinned_bypass_with_held_native_mutation(
            src,
            offset,
            len,
            try_async,
            held_native_writeback_gate(&native_mutation),
        )
    }

    pub(super) fn try_write_aligned_pinned_bypass_with_held_native_mutation(
        &self,
        src: &[PinnedPhysicalSegment],
        offset: u64,
        len: usize,
        _try_async: bool,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<Option<usize>> {
        if self.in_memory {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_IN_MEMORY, 1);
            return Ok(None);
        }
        let Some(pages) = Self::aligned_page_range(offset, len) else {
            record_cached_file_counter(&WRITE_BYPASS_REJECT_UNALIGNED, 1);
            return Ok(None);
        };
        record_cached_file_counter(&WRITE_BYPASS_ELIGIBLE, 1);

        // Own the complete invalidation domain before copying from the pinned
        // source through kernel-owned bounce storage.
        let _direct_guard = self.shared.direct_io_lock.lock();
        let _writeback_guard = self.shared.writeback_lock.write();
        let mutation = match self.begin_cache_invalidating_mutation() {
            Ok(mutation) => mutation,
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        };
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        match self.invalidate_cached_range_with_held_native_gate(
            file,
            pages.clone(),
            &mutation,
            native_gate,
        ) {
            Ok(_) => {}
            // The physical source may itself be a precisely pinned target
            // cache page. Preserve it and use the overlap-aware cached path.
            Err(VfsError::ResourceBusy) => return Ok(None),
            Err(error) => return Err(error),
        }

        // Pinned user pages are stable but not Rust-shared or exclusive.
        let written = unsafe { write_file_from_pinned_bounce(file, src, offset, len) }?;

        self.invalidate_cached_range_with_held_native_gate(file, pages, &mutation, native_gate)?;
        if written > 0 {
            record_cached_file_counter(&WRITE_BYPASS_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_BYTES, written as u64);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_HITS, 1);
            record_cached_file_counter(&WRITE_BYPASS_SLICE_BYTES, written as u64);
        }
        Ok(Some(written))
    }

    /// Allocates, reads, and initializes one cache window without holding the
    /// page-cache index. Publication is a separate short critical section so
    /// readers cannot observe a partially filled page.
    fn prepare_page_window(
        &self,
        file: &FileNode,
        range: Range<u64>,
        allow_async_page_fill: bool,
    ) -> VfsResult<Vec<(u32, PageCache)>> {
        let count = usize::try_from(range.end - range.start).map_err(|_| VfsError::InvalidInput)?;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(count)
            .map_err(|_| VfsError::NoMemory)?;
        for _ in 0..count {
            let mut page = PageCache::new(self.shared.in_memory)?;
            page.data().fill(0);
            pages.push(page);
        }

        let offset = range.start * PAGE_SIZE as u64;
        let async_page_fill = allow_async_page_fill && {
            #[cfg(feature = "ext4")]
            {
                lwext4_rust::async_mapped_read_enabled()
            }
            #[cfg(not(feature = "ext4"))]
            {
                false
            }
        };
        let read = {
            let mut bufs = Vec::new();
            bufs.try_reserve_exact(count)
                .map_err(|_| VfsError::NoMemory)?;
            bufs.extend(pages.iter_mut().map(PageCache::data));
            if async_page_fill {
                match file.try_read_at_vectored_async(&mut bufs, offset)? {
                    Some(read) => read,
                    None => file.read_at_vectored(&mut bufs, offset)?,
                }
            } else {
                file.read_at_vectored(&mut bufs, offset)?
            }
        };
        let bytes = count.checked_mul(PAGE_SIZE).ok_or(VfsError::InvalidInput)?;
        if read > bytes {
            return Err(VfsError::Io);
        }
        if !self.shared.in_memory {
            crate::account_backing_read(read);
        }

        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(count)
            .map_err(|_| VfsError::NoMemory)?;
        for (index, page) in pages.into_iter().enumerate() {
            let byte_offset = index * PAGE_SIZE;
            if index != 0 && byte_offset >= read {
                break;
            }
            let number = range
                .start
                .checked_add(index as u64)
                .and_then(|number| u32::try_from(number).ok())
                .ok_or(VfsError::InvalidInput)?;
            prepared.push((number, page));
        }
        #[cfg(feature = "ext4")]
        if async_page_fill {
            lwext4_rust::record_readahead_async_pages(read.div_ceil(PAGE_SIZE).min(prepared.len()));
        }
        Ok(prepared)
    }

    fn publish_page_window(
        &self,
        cache: &mut LruCache<u32, PageCache>,
        mut pages: Vec<(u32, PageCache)>,
        demand: u32,
        readahead_enabled: bool,
    ) -> VfsResult<usize> {
        // Publish the demand page first. Readahead is best effort and must not
        // consume the last available cache slot ahead of the requested page.
        if let Some(index) = pages.iter().position(|(number, _)| *number == demand) {
            let page = pages.remove(index);
            pages.insert(0, page);
        }
        let mut loaded_readahead_pages = 0usize;
        let mut demand_inserted = false;
        for (number, mut page) in pages {
            if cache.contains(&number) {
                continue;
            }
            if cache.len() >= cache.cap().get() {
                if number == demand {
                    return Err(VfsError::ResourceBusy);
                }
                continue;
            }
            if number != demand {
                page.mark_prefetched();
            }
            file_cache_apply_refault(&self.shared, number, &mut page);
            if cache.put(number, page).is_some() {
                unreachable!("cache-fill publication replaced an existing page");
            }
            file_cache_resident_add(1);
            demand_inserted |= number == demand;
            if number != demand {
                cache.demote(&number);
                loaded_readahead_pages += 1;
            }
        }
        if readahead_enabled {
            record_readahead_window(loaded_readahead_pages);
        }
        Ok(usize::from(demand_inserted))
    }

    fn populate_missing_page(
        &self,
        file: &FileNode,
        pn: u32,
        load_from_file: bool,
        allow_async_page_fill: bool,
        readahead_pages: usize,
    ) -> VfsResult<()> {
        let readahead_enabled = load_from_file && readahead_pages > 1 && cached_readahead_enabled();
        if readahead_enabled {
            record_readahead_miss();
        }
        let requested_pages = if readahead_enabled {
            readahead_pages.min(MMAP_SEQUENTIAL_READAHEAD_PAGES)
        } else {
            1
        };

        loop {
            let capacity_epoch = self.shared.pending_page_fills.capacity_epoch();
            let admission =
                self.shared
                    .pending_page_fills
                    .admit(&self.shared, pn, requested_pages)?;
            match admission {
                PendingPageFillAdmission::Present => return Ok(()),
                PendingPageFillAdmission::Wait(fill) => fill.wait()?,
                PendingPageFillAdmission::Capacity => {
                    self.shared
                        .pending_page_fills
                        .wait_for_capacity(capacity_epoch)?;
                }
                PendingPageFillAdmission::Full => return Err(VfsError::ResourceBusy),
                PendingPageFillAdmission::Load(owner) => {
                    let range = owner.range();
                    let prepared = if load_from_file {
                        self.prepare_page_window(file, range, allow_async_page_fill)
                    } else {
                        PageCache::new(self.shared.in_memory).and_then(|mut page| {
                            page.data().fill(0);
                            let mut pages = Vec::new();
                            pages.try_reserve_exact(1).map_err(|_| VfsError::NoMemory)?;
                            pages.push((pn, page));
                            Ok(pages)
                        })
                    };
                    let result = match prepared {
                        Ok(pages) => {
                            let mut cache = self.shared.page_cache.lock();
                            let result =
                                self.publish_page_window(&mut cache, pages, pn, readahead_enabled);
                            if !load_from_file
                                && result.as_ref().is_ok_and(|inserted| *inserted != 0)
                            {
                                record_cached_file_counter(&WRITE_NO_READ_INSERT_PAGES, 1);
                                record_cached_file_counter(
                                    &WRITE_NO_READ_INSERT_BYTES,
                                    PAGE_SIZE as u64,
                                );
                            }
                            drop(cache);
                            owner.finish(result.map(|_| ()))
                        }
                        Err(error) => owner.finish(Err(error)),
                    };
                    result?;
                    return Ok(());
                }
            }
        }
    }

    /// Observes residency without accessing page data or waiting for writeback.
    pub fn is_page_cached(&self, pn: u32) -> bool {
        self.shared.page_cache.lock().contains(&pn)
    }

    /// Accesses a cached page without waiting for writeback. Callers holding an
    /// address-space lock must release it before retrying `ResourceBusy`.
    /// A missing page is passed to the callback as `None`; a busy page never is.
    pub fn try_with_page<R>(
        &self,
        pn: u32,
        f: impl FnOnce(Option<&mut PageCache>) -> R,
    ) -> VfsResult<R> {
        let _range_lease = CachedFileShared::try_range_cache_lease(
            &self.shared,
            page_range(u64::from(pn), 1),
            RangeCacheLeaseKind::CachedWrite,
        )?;
        let mut guard = self.shared.page_cache.lock();
        if guard.get(&pn).is_some_and(PageCache::is_writeback) {
            return Err(VfsError::ResourceBusy);
        }
        if let Some(page) = guard.get_mut(&pn) {
            file_cache_record_page_reference(page);
        }
        let result = f(guard.get_mut(&pn));
        let dirty = guard.get(&pn).is_some_and(PageCache::is_dirty);
        drop(guard);
        if dirty {
            retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
        }
        Ok(result)
    }

    /// Invokes `f` with the cached page at `pn`, or `None` if it is not cached.
    pub fn with_page<R>(&self, pn: u32, f: impl FnOnce(Option<&mut PageCache>) -> R) -> R {
        let mut f = Some(f);
        let _range_lease = match CachedFileShared::try_range_cache_lease(
            &self.shared,
            page_range(u64::from(pn), 1),
            RangeCacheLeaseKind::CachedWrite,
        ) {
            Ok(lease) => Some(lease),
            Err(_) => return f.take().unwrap()(None),
        };
        loop {
            let mut guard = self.shared.page_cache.lock();
            if guard.get(&pn).is_some_and(PageCache::is_writeback) {
                drop(guard);
                wait_for_page_writeback_clear(&self.shared, pn);
                continue;
            }
            if let Some(page) = guard.get_mut(&pn) {
                file_cache_record_page_reference(page);
            }
            let result = f.take().unwrap()(guard.get_mut(&pn));
            let dirty = guard.get(&pn).is_some_and(PageCache::is_dirty);
            drop(guard);
            if dirty {
                retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
            }
            return result;
        }
    }

    /// Invokes `f` with the cached page at `pn`, loading it from disk if absent.
    /// Cache pressure uses staged reclamation with all cache locks dropped.
    pub fn with_page_or_insert<R>(
        &self,
        pn: u32,
        f: impl FnOnce(&mut PageCache) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let mut f = Some(f);
        let mut reclaimed = 0usize;
        loop {
            let attempt = self.with_page_or_insert_without_reclaim_with_options(
                pn,
                true,
                true,
                READAHEAD_PAGES,
                true,
                |page| f.take().expect("page callback was consumed")(page),
            );
            match attempt {
                Ok(value) => return Ok(value),
                Err(VfsError::ResourceBusy) if f.is_some() => {
                    let (cache_full, writeback) = {
                        let cache = self.shared.page_cache.lock();
                        (
                            !cache.contains(&pn) && cache.len() == cache.cap().get(),
                            cache.peek(&pn).is_some_and(PageCache::is_writeback),
                        )
                    };
                    if writeback {
                        wait_for_page_writeback_clear(&self.shared, pn);
                        continue;
                    }
                    if !cache_full || reclaimed == LOCK_EXTERNAL_INSERT_RECLAIM_RETRY_LIMIT {
                        return Err(VfsError::ResourceBusy);
                    }
                    // Reclaim only after the cache-user guard from the failed
                    // insertion attempt has dropped. This keeps independent
                    // buffered users concurrent while retaining staged alias
                    // teardown at capacity.
                    if !self.reclaim_one()? {
                        return Err(VfsError::ResourceBusy);
                    }
                    reclaimed += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Invokes `f` with a cached page, loading it only when doing so does not
    /// require cache replacement.
    ///
    /// This is for callers which currently hold an address-space lock.  Cache
    /// replacement may prepare aliases in *another* address space, so doing it
    /// from that lock would invert the cache-to-address-space lock order.  A
    /// full cache is therefore reported as `ResourceBusy`; the MM layer uses
    /// that internal result to drop its address-space lock, reclaim one page
    /// transactionally, and retry after revalidation.
    pub fn with_page_or_insert_without_reclaim<R>(
        &self,
        pn: u32,
        f: impl FnOnce(&mut PageCache) -> VfsResult<R>,
    ) -> VfsResult<R> {
        self.with_page_or_insert_without_reclaim_with_readahead(pn, 1, f)
    }

    /// Loads a page from task context, reclaiming through a fully staged
    /// cache-eviction transaction when the cache is full.
    ///
    /// Insertion is first attempted under the direct-I/O exclusion lock
    /// without replacement. On a full cache both locks have been dropped
    /// before [`Self::reclaim_one`] stages a page, prepares every alias
    /// reservation, and commits its eviction. The insertion is then retried
    /// from scratch. Callers must not hold an mm lock, because reclamation
    /// prepares mappings in arbitrary address spaces.
    pub fn with_page_or_insert_lock_external_reclaim<R>(
        &self,
        pn: u32,
        f: impl FnOnce(&mut PageCache) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let mut f = Some(f);
        let mut reclaimed = 0usize;
        loop {
            let attempt = {
                let _direct_guard = self.shared.direct_io_lock.lock();
                self.with_page_or_insert_without_reclaim(pn, |page| {
                    f.take().expect("page insertion callback was consumed")(page)
                })
            };
            match attempt {
                Ok(value) => return Ok(value),
                Err(VfsError::ResourceBusy) if f.is_some() => {
                    // No cache or direct-I/O mutex is held here.  The staged
                    // pageout path publishes reversible alias fences before
                    // it can ever take an address-space mutex.
                    if reclaimed == LOCK_EXTERNAL_INSERT_RECLAIM_RETRY_LIMIT {
                        return Err(VfsError::ResourceBusy);
                    }
                    if !self.reclaim_one()? {
                        return Err(VfsError::ResourceBusy);
                    }
                    reclaimed += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Inserts or accesses a page without invoking an eviction listener while
    /// holding the cache mutex. When `native_gate` is `Some`, the caller must
    /// already hold this inode's `direct_io_lock` after acquiring the source
    /// writeback gate; cache pressure is then reclaimed through the staged
    /// eviction protocol. With no native gate this never reclaims, so it is
    /// safe under an mm lock and returns `ResourceBusy` at capacity.
    pub(super) fn with_page_or_insert_with_reclaim<R>(
        &self,
        pn: u32,
        load_from_file: bool,
        allow_async_page_fill: bool,
        readahead_pages: usize,
        wait_writeback: bool,
        native_gate: Option<HeldNativeWritebackGate<'_>>,
        f: impl FnOnce(&mut PageCache) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let mut f = Some(f);
        let mut reclaimed = 0usize;
        loop {
            let attempt = self.with_page_or_insert_without_reclaim_with_options(
                pn,
                load_from_file,
                allow_async_page_fill,
                readahead_pages,
                wait_writeback,
                |page| f.take().expect("page insertion callback was consumed")(page),
            );
            match attempt {
                Ok(value) => return Ok(value),
                Err(VfsError::ResourceBusy) if f.is_some() => {
                    let (cache_full, writeback) = {
                        let cache = self.shared.page_cache.lock();
                        (
                            !cache.contains(&pn) && cache.len() == cache.cap().get(),
                            cache.peek(&pn).is_some_and(PageCache::is_writeback),
                        )
                    };
                    if wait_writeback && writeback {
                        wait_for_page_writeback_clear(&self.shared, pn);
                        continue;
                    }
                    if !cache_full {
                        return Err(VfsError::ResourceBusy);
                    }
                    let Some(native_gate) = native_gate else {
                        return Err(VfsError::ResourceBusy);
                    };
                    if reclaimed == LOCK_EXTERNAL_INSERT_RECLAIM_RETRY_LIMIT {
                        return Err(VfsError::ResourceBusy);
                    }
                    // The direct-I/O lock is held by our caller, but all
                    // cache-user, range-lease and page-cache guards from the
                    // failed insertion attempt have been dropped.
                    if !self.reclaim_one_with_held_direct_io_lock(native_gate)? {
                        return Err(VfsError::ResourceBusy);
                    }
                    reclaimed += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Loads one mmap fault page and an optional forward page-cache window
    /// without replacing any existing cache page.
    ///
    /// The caller may hold an address-space lock.  A full cache therefore
    /// remains an internal `ResourceBusy` retry signal; even the speculative
    /// window is capped to currently unused slots and can never enter alias
    /// eviction while that lock is held.
    pub fn with_page_or_insert_without_reclaim_with_readahead<R>(
        &self,
        pn: u32,
        readahead_pages: usize,
        f: impl FnOnce(&mut PageCache) -> VfsResult<R>,
    ) -> VfsResult<R> {
        self.with_page_or_insert_without_reclaim_with_options(
            pn,
            true,
            false,
            readahead_pages,
            true,
            f,
        )
    }

    fn with_page_or_insert_without_reclaim_with_options<R>(
        &self,
        pn: u32,
        load_from_file: bool,
        allow_async_page_fill: bool,
        readahead_pages: usize,
        reject_writeback: bool,
        f: impl FnOnce(&mut PageCache) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let _cache_user = self.begin_cache_user_range(
            page_range(u64::from(pn), 1),
            RangeCacheLeaseKind::CachedWrite,
        )?;
        let file = self.inner.entry().as_file()?;
        let mut f = Some(f);
        loop {
            {
                let mut cache = self.shared.page_cache.lock();
                if let Some(page) = cache.get_mut(&pn) {
                    if page.clear_prefetched() && load_from_file {
                        record_readahead_hit();
                    }
                    file_cache_record_page_reference(page);
                    if reject_writeback && page.is_writeback() {
                        return Err(VfsError::ResourceBusy);
                    } else {
                        let result = f.take().unwrap()(page);
                        let dirty = cache.get(&pn).is_some_and(PageCache::is_dirty);
                        drop(cache);
                        if dirty {
                            retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
                        }
                        return result;
                    }
                } else {
                    if cache.len() >= cache.cap().get() {
                        return Err(VfsError::ResourceBusy);
                    }
                }
            }
            self.populate_missing_page(
                file,
                pn,
                load_from_file,
                allow_async_page_fill,
                readahead_pages.max(1),
            )?;
        }
    }

    /// Runs `f` while direct I/O is excluded from this inode's page cache.
    pub fn with_direct_io_excluded<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self.shared.direct_io_lock.lock();
        f()
    }
}
