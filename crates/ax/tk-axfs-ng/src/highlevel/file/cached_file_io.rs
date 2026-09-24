//! CachedFile: buffered reads, writes, appends and truncation.

use super::*;

impl CachedFile {
    pub(super) fn with_pages<T>(
        &self,
        range: Range<u64>,
        page_initial: impl FnOnce(&FileNode) -> VfsResult<T>,
        mut load_page: impl FnMut(u64, &Range<usize>) -> bool,
        mut page_each: impl FnMut(T, &mut PageCache, u64, Range<usize>) -> VfsResult<T>,
        wait_writeback: bool,
        allow_async_page_fill: bool,
        readahead: bool,
    ) -> VfsResult<T> {
        let _cache_user =
            self.begin_cache_user_range(range.clone(), RangeCacheLeaseKind::CachedWrite)?;
        let file = self.inner.entry().as_file()?;
        let mut initial = page_initial(file)?;
        let start_page =
            u32::try_from(range.start / PAGE_SIZE as u64).map_err(|_| VfsError::InvalidInput)?;
        let end_page = u32::try_from(range.end.div_ceil(PAGE_SIZE as u64))
            .map_err(|_| VfsError::InvalidInput)?;
        let mut page_offset = (range.start % PAGE_SIZE as u64) as usize;
        for pn in start_page..end_page {
            let page_start = pn as u64 * PAGE_SIZE as u64;
            loop {
                let mut guard = self.shared.page_cache.lock();
                let page_range =
                    page_offset..(range.end - page_start).min(PAGE_SIZE as u64) as usize;
                let load_from_file = load_page(page_start, &page_range);
                self.ensure_page_cached_with(
                    file,
                    &mut guard,
                    pn,
                    load_from_file,
                    allow_async_page_fill,
                    readahead,
                )?;
                if wait_writeback && guard.get(&pn).is_some_and(PageCache::is_writeback) {
                    drop(guard);
                    wait_for_page_writeback_clear(&self.shared, pn);
                    continue;
                }
                let page = guard.get_mut(&pn).unwrap();
                initial = page_each(initial, page, page_start, page_range)?;
                break;
            }
            page_offset = 0;
        }

        Ok(initial)
    }

    pub(super) fn prepare_write_page(
        &self,
        file: &FileNode,
        pn: u32,
        load_from_file: bool,
        allow_async_page_fill: bool,
    ) -> VfsResult<CachedFilePagePin> {
        let _cache_user = self.begin_cache_user()?;
        loop {
            let mut guard = self.shared.page_cache.lock();
            let evicted = self.ensure_page_cached_with(
                file,
                &mut guard,
                pn,
                load_from_file,
                allow_async_page_fill,
                true,
            )?;
            if guard.get(&pn).is_some_and(PageCache::is_writeback) {
                drop(evicted);
                drop(guard);
                wait_for_page_writeback_clear(&self.shared, pn);
                continue;
            }
            let range_lease = Some(CachedFileShared::try_range_cache_lease(
                &self.shared,
                page_range(u64::from(pn), 1),
                RangeCacheLeaseKind::CachedWrite,
            )?);
            guard
                .get_mut(&pn)
                .expect("prepared cache page disappeared while locked")
                .pin()?;
            drop(guard);
            drop(evicted);
            return Ok(CachedFilePagePin {
                cache: self.clone(),
                pn,
                dirty_on_release: false,
                _range_lease: range_lease,
            });
        }
    }

    pub(super) fn commit_prepared_write(&self, pn: u32, range: Range<usize>, src: &[u8]) {
        loop {
            let mut guard = self.shared.page_cache.lock();
            if guard.get(&pn).is_some_and(PageCache::is_writeback) {
                drop(guard);
                wait_for_page_writeback_clear(&self.shared, pn);
                continue;
            }
            let page = guard
                .get_mut(&pn)
                .expect("pinned cache page disappeared before write commit");
            page.data()[range].copy_from_slice(src);
            page.mark_dirty();
            return;
        }
    }

    pub(super) fn read_at_with_async_policy_with_bounce(
        &self,
        mut dst: impl Write + IoBufMut,
        offset: u64,
        allow_async: bool,
        readahead: bool,
        bounce: &mut [u8],
    ) -> VfsResult<usize> {
        if bounce.len() < PAGE_SIZE {
            return Err(VfsError::InvalidInput);
        }
        let len = self.inner.len()?;
        self.shared.observe_file_len(len);
        let requested = u64::try_from(dst.remaining_mut()).map_err(|_| VfsError::InvalidInput)?;
        let end = offset
            .checked_add(requested)
            .ok_or(VfsError::InvalidInput)?
            .min(len);
        if end <= offset {
            return Ok(0);
        }
        if allow_async {
            if let Some(read) =
                self.try_read_aligned_bypass(&mut dst, offset, (end - offset) as usize)?
            {
                return Ok(read);
            }
        }
        let _direct_guard = self.shared.direct_io_lock.lock();
        let mut total = 0usize;
        let mut current = offset;
        while current < end {
            let page_offset = (current % PAGE_SIZE as u64) as usize;
            let chunk = usize::try_from(end - current)
                .map_err(|_| VfsError::InvalidInput)?
                .min(PAGE_SIZE - page_offset);
            let next = current
                .checked_add(chunk as u64)
                .ok_or(VfsError::InvalidInput)?;
            let copied = match self.with_pages(
                current..next,
                |_| Ok(0usize),
                |_, _| true,
                |_, page, _page_start, range| {
                    let copied = range.end - range.start;
                    bounce[..copied].copy_from_slice(&page.data()[range]);
                    Ok(copied)
                },
                false,
                allow_async,
                readahead,
            ) {
                Ok(copied) => copied,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            // The page-cache borrow and its lock have ended before an IoBufMut
            // such as VmBytesMut performs a raw userspace copy.
            let written = match dst.write(&bounce[..copied]) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            if written > copied {
                return Err(VfsError::InvalidInput);
            }
            total += written;
            current += written as u64;
            if written < copied || written == 0 {
                break;
            }
        }
        Ok(total)
    }

    pub(super) fn read_at_with_async_policy(
        &self,
        dst: impl Write + IoBufMut,
        offset: u64,
        allow_async: bool,
        readahead: bool,
    ) -> VfsResult<usize> {
        let mut bounce = Vec::new();
        bounce
            .try_reserve_exact(PAGE_SIZE)
            .map_err(|_| VfsError::NoMemory)?;
        bounce.resize(PAGE_SIZE, 0);
        self.read_at_with_async_policy_with_bounce(dst, offset, allow_async, readahead, &mut bounce)
    }

    /// Reads data from the file at `offset` into `dst`.
    pub fn read_at(&self, dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        self.read_at_with_async_policy(dst, offset, true, true)
    }

    /// Reads with caller-selected automatic read-ahead.  The policy belongs
    /// to the open file description, never to this inode-shared cache.
    pub fn read_at_with_readahead(
        &self,
        dst: impl Write + IoBufMut,
        offset: u64,
        readahead: bool,
    ) -> VfsResult<usize> {
        self.read_at_with_async_policy(dst, offset, true, readahead)
    }

    /// Reads through the coherent page cache without invoking the lower
    /// split-submit asynchronous hook. A synchronous lower call may still
    /// submit a device request, but it completes that request before returning.
    ///
    /// This is intended for callers that already own a larger transaction and
    /// therefore cannot release its transaction and suspend after publishing a
    /// request. Ordinary reads should use [`read_at`](Self::read_at), which may
    /// use the explicit split-submit/wait path when the filesystem supports it.
    pub fn read_at_sync(&self, dst: impl Write + IoBufMut, offset: u64) -> VfsResult<usize> {
        self.read_at_with_async_policy(dst, offset, false, true)
    }

    /// Cache-managed synchronous read using caller-reserved bounce storage.
    /// This is used after owned-I/O publication so worker execution never
    /// allocates a page bounce or enters an aligned direct-bypass path.
    pub(super) fn read_at_sync_with_bounce(
        &self,
        dst: impl Write + IoBufMut,
        offset: u64,
        bounce: &mut [u8],
    ) -> VfsResult<usize> {
        self.read_at_with_async_policy_with_bounce(dst, offset, false, true, bounce)
    }

    /// Reads into caller-pinned physical memory without exposing an aliasing
    /// Rust slice when the destination is one of this inode's cached pages.
    ///
    /// # Safety
    ///
    /// Every nonempty destination range must remain pinned, mapped, writable,
    /// and accessible for the complete call. Destination ranges must not
    /// overlap each other. Concurrent userspace access is permitted; this path
    /// creates no Rust reference to the physical ranges and copies through
    /// kernel-owned storage. `try_async` is a hint and may be ignored.
    pub unsafe fn read_at_pinned_segments(
        &self,
        dst: &[PinnedPhysicalSegment],
        offset: u64,
        try_async: bool,
    ) -> VfsResult<usize> {
        let requested = validate_pinned_physical_segments(dst, true)?;
        let file_len = self.inner.len()?;
        let end = offset
            .checked_add(u64::try_from(requested).map_err(|_| VfsError::InvalidInput)?)
            .ok_or(VfsError::InvalidInput)?
            .min(file_len);
        if end <= offset {
            return Ok(0);
        }
        let len = usize::try_from(end - offset).map_err(|_| VfsError::InvalidInput)?;
        if let Some(read) = self.try_read_aligned_pinned_bypass(dst, offset, len, try_async)? {
            return Ok(read);
        }

        let _direct_guard = self.shared.direct_io_lock.lock();
        let mut cursor = PinnedPhysicalCursor::new(dst);
        let mut bounce = try_zeroed_pinned_io_bounce(PAGE_SIZE)?;
        let mut total = 0usize;
        let mut current = offset;
        while current < end {
            let page_offset = (current % PAGE_SIZE as u64) as usize;
            let chunk = usize::try_from(end - current)
                .map_err(|_| VfsError::InvalidInput)?
                .min(PAGE_SIZE - page_offset);
            let next = current
                .checked_add(chunk as u64)
                .ok_or(VfsError::InvalidInput)?;
            let copied = match self.with_pages(
                current..next,
                |_| Ok(0usize),
                |_, _| true,
                |_, page, _page_start, range| {
                    let copied = range.end - range.start;
                    bounce[..copied].copy_from_slice(&page.data()[range]);
                    Ok(copied)
                },
                false,
                try_async,
                true,
            ) {
                Ok(copied) => copied,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            match unsafe { copy_to_pinned_physical_segments(&mut cursor, &bounce[..copied]) } {
                Ok(()) => {}
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            }
            total += copied;
            current = next;
        }
        Ok(total)
    }

    pub fn read_at_slice(&self, mut dst: &mut [u8], offset: u64) -> VfsResult<usize> {
        let len = self.inner.len()?;
        let end = (offset + dst.len() as u64).min(len);
        if end <= offset {
            return Ok(0);
        }
        dst = &mut dst[..(end - offset) as usize];
        if let Some(read) = self.try_read_aligned_slice_bypass(dst, offset)? {
            return Ok(read);
        }
        self.read_at(&mut dst, offset)
    }

    pub fn read_at_vectored(&self, dst: &mut [&mut [u8]], offset: u64) -> VfsResult<usize> {
        if let Some(read) = self.try_read_aligned_vectored_bypass(dst, offset)? {
            return Ok(read);
        }
        let mut total = 0usize;
        let mut current = offset;
        for buf in dst.iter_mut() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let read = match self.read_at_slice(buf, current) {
                Ok(read) => read,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += read;
            current += read as u64;
            if read < requested || read == 0 {
                break;
            }
        }
        Ok(total)
    }

    pub(super) fn write_at_locked(
        &self,
        mut buf: impl Read + IoBuf,
        offset: u64,
    ) -> VfsResult<usize> {
        let mut bounce = Vec::new();
        bounce
            .try_reserve_exact(PAGE_SIZE)
            .map_err(|_| VfsError::NoMemory)?;
        bounce.resize(PAGE_SIZE, 0);
        self.write_at_locked_with_bounce(&mut buf, offset, &mut bounce)
    }

    /// Cache-owned write under the caller's direct/append locks, using a
    /// prepare-time page bounce.  No allocation is permitted after an owned
    /// request has crossed its publication boundary.
    pub(super) fn write_at_locked_with_bounce(
        &self,
        mut buf: impl Read + IoBuf,
        offset: u64,
        bounce: &mut [u8],
    ) -> VfsResult<usize> {
        if bounce.len() < PAGE_SIZE {
            return Err(VfsError::InvalidInput);
        }
        let requested = buf.remaining();
        offset
            .checked_add(u64::try_from(requested).map_err(|_| VfsError::InvalidInput)?)
            .ok_or(VfsError::InvalidInput)?;
        let file = self.inner.entry().as_file()?;
        let mut committed_len = file.len()?;
        self.shared.observe_file_len(committed_len);

        let mut written = 0usize;
        let mut current = offset;
        while written < requested {
            let page_offset = (current % PAGE_SIZE as u64) as usize;
            let chunk = (requested - written).min(PAGE_SIZE - page_offset);
            // Finish a generic source read before acquiring a cache page and
            // creating its mutable data reference. VmBytes may point at that
            // exact cached page through a MAP_SHARED mapping.
            let read = match buf.read(&mut bounce[..chunk]) {
                Ok(read) => read,
                Err(_) if written != 0 => break,
                Err(error) => return Err(error),
            };
            if read > chunk {
                if written != 0 {
                    break;
                }
                return Err(VfsError::InvalidInput);
            }
            if read == 0 {
                break;
            }
            let next = current
                .checked_add(read as u64)
                .ok_or(VfsError::InvalidInput)?;
            let pn =
                u32::try_from(current / PAGE_SIZE as u64).map_err(|_| VfsError::InvalidInput)?;
            let range = page_offset..page_offset + read;
            let load_from_file = !(range.start == 0 && range.end == PAGE_SIZE)
                && u64::from(pn) * (PAGE_SIZE as u64) < committed_len;
            let page_pin = match self.prepare_write_page(file, pn, load_from_file, true) {
                Ok(page_pin) => page_pin,
                Err(_) if written != 0 => break,
                Err(error) => return Err(error),
            };
            if next > committed_len {
                match file.set_len(next) {
                    Ok(()) => {
                        committed_len = next;
                        self.shared.observe_file_len(committed_len);
                    }
                    Err(_) if written != 0 => break,
                    Err(error) => return Err(error),
                }
            }
            self.commit_prepared_write(pn, range, &bounce[..read]);
            drop(page_pin);
            written += read;
            current = next;
            if read < chunk {
                break;
            }
        }
        if written != 0 {
            retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
        }
        Ok(written)
    }

    /// Writes from caller-pinned physical memory without exposing an aliasing
    /// Rust slice when a source range is one of this inode's cached pages.
    ///
    /// # Safety
    ///
    /// Every nonempty source range must remain pinned, mapped, readable, and
    /// accessible for the complete call. Concurrent userspace writes may race
    /// with the raw copy, but no Rust reference is created for the physical
    /// range. `try_async` is a hint and may be ignored.
    pub unsafe fn write_at_pinned_segments(
        &self,
        src: &[PinnedPhysicalSegment],
        offset: u64,
        try_async: bool,
    ) -> VfsResult<usize> {
        let native_mutation = begin_source_location_writeback_mutation(&self.inner)?;
        unsafe {
            self.write_at_pinned_segments_with_held_native_mutation(
                src,
                offset,
                try_async,
                held_native_writeback_gate(&native_mutation),
            )
        }
    }

    pub(super) unsafe fn write_at_pinned_segments_with_held_native_mutation(
        &self,
        src: &[PinnedPhysicalSegment],
        offset: u64,
        try_async: bool,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<usize> {
        let requested = validate_pinned_physical_segments(src, false)?;
        if requested == 0 {
            return Ok(0);
        }
        offset
            .checked_add(u64::try_from(requested).map_err(|_| VfsError::InvalidInput)?)
            .ok_or(VfsError::InvalidInput)?;
        if let Some(written) = self.try_write_aligned_pinned_bypass_with_held_native_mutation(
            src,
            offset,
            requested,
            try_async,
            native_gate,
        )? {
            return Ok(written);
        }

        let _direct_guard = self.shared.direct_io_lock.lock();
        let _append_guard = self.shared.append_lock.read();
        let file = self.inner.entry().as_file()?;
        let mut committed_len = file.len()?;
        self.shared.observe_file_len(committed_len);

        let mut cursor = PinnedPhysicalCursor::new(src);
        let mut bounce = try_zeroed_pinned_io_bounce(PAGE_SIZE)?;
        let mut written = 0usize;
        let mut current = offset;
        while written < requested {
            let page_offset = (current % PAGE_SIZE as u64) as usize;
            let chunk = (requested - written).min(PAGE_SIZE - page_offset);
            match unsafe { copy_from_pinned_physical_segments(&mut cursor, &mut bounce[..chunk]) } {
                Ok(()) => {}
                Err(_) if written != 0 => break,
                Err(error) => return Err(error),
            }
            let next = current
                .checked_add(chunk as u64)
                .ok_or(VfsError::InvalidInput)?;
            let pn =
                u32::try_from(current / PAGE_SIZE as u64).map_err(|_| VfsError::InvalidInput)?;
            let range = page_offset..page_offset + chunk;
            let load_from_file = !(range.start == 0 && range.end == PAGE_SIZE)
                && u64::from(pn) * (PAGE_SIZE as u64) < committed_len;
            let page_pin = match self.prepare_write_page(file, pn, load_from_file, try_async) {
                Ok(page_pin) => page_pin,
                Err(_) if written != 0 => break,
                Err(error) => return Err(error),
            };
            if next > committed_len {
                match file.set_len(next) {
                    Ok(()) => {
                        committed_len = next;
                        self.shared.observe_file_len(committed_len);
                    }
                    Err(_) if written != 0 => break,
                    Err(error) => return Err(error),
                }
            }
            self.commit_prepared_write(pn, range, &bounce[..chunk]);
            drop(page_pin);
            written += chunk;
            current = next;
        }
        if written != 0 {
            retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
        }
        Ok(written)
    }

    /// Writes `buf` to the file at `offset`.
    pub fn write_at(&self, mut buf: impl Read + IoBuf, offset: u64) -> VfsResult<usize> {
        let len = buf.remaining();
        if let Some(written) = self.try_write_aligned_bypass(&mut buf, offset, len)? {
            return Ok(written);
        }
        let _direct_guard = self.shared.direct_io_lock.lock();
        let _guard = self.shared.append_lock.read();
        self.write_at_locked(buf, offset)
    }

    pub(super) fn write_at_with_held_native_mutation(
        &self,
        mut buf: impl Read + IoBuf,
        offset: u64,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<usize> {
        let len = buf.remaining();
        if let Some(written) = self.try_write_aligned_bypass_with_held_native_mutation(
            &mut buf,
            offset,
            len,
            native_gate,
        )? {
            return Ok(written);
        }
        let _direct_guard = self.shared.direct_io_lock.lock();
        let _guard = self.shared.append_lock.read();
        self.write_at_locked(buf, offset)
    }

    pub fn write_at_slice(&self, src: &[u8], offset: u64) -> VfsResult<usize> {
        if let Some(written) = self.try_write_aligned_slice_bypass(src, offset)? {
            return Ok(written);
        }
        self.write_at(src, offset)
    }

    pub fn write_at_vectored(&self, src: &[&[u8]], offset: u64) -> VfsResult<usize> {
        if let Some(written) = self.try_write_aligned_vectored_bypass(src, offset)? {
            return Ok(written);
        }
        let mut total = 0usize;
        let mut current = offset;
        for buf in src.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let written = match self.write_at_slice(buf, current) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += written;
            current += written as u64;
            if written < requested || written == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Appends an admitted prefix of `buf` under one inode append transaction.
    ///
    /// `admit` observes the exact end offset protected by the same append
    /// serialization domain as the lower write. It may shorten the operation,
    /// but cannot extend it beyond the caller's remaining input.
    pub fn append_with_admission(
        &self,
        mut buf: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<(usize, u64)> {
        let _direct_guard = self.shared.direct_io_lock.lock();
        let _guard = self.shared.append_lock.write();
        let file = self.inner.entry().as_file()?;
        let len = file.len()?;
        let requested = buf.remaining();
        let allowed = admit(len, requested)?;
        if allowed > requested {
            return Err(VfsError::InvalidInput);
        }
        let mut admitted = (&mut buf).take(allowed as u64);
        let written = self.write_at_locked(&mut admitted, len)?;
        let new_end = len
            .checked_add(written as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok((written, new_end))
    }

    pub(super) fn append_with_admission_with_held_native_mutation(
        &self,
        src: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
        _native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<(usize, u64)> {
        self.append_with_admission(src, admit)
    }

    /// Nonblocking append transaction for RWF_NOWAIT. Both inode gates are
    /// acquired with try-lock before the caller's admission closure or any
    /// cache mutation; contention is reported as `None`.
    pub fn try_append_with_admission(
        &self,
        mut buf: impl Read + IoBuf,
        admit: impl FnOnce(u64, usize) -> VfsResult<usize>,
    ) -> VfsResult<Option<(usize, u64)>> {
        let Some(_direct_guard) = self.shared.direct_io_lock.try_lock() else {
            return Ok(None);
        };
        let Some(_append_guard) = self.shared.append_lock.try_write() else {
            return Ok(None);
        };
        let file = self.inner.entry().as_file()?;
        let len = file.len()?;
        let requested = buf.remaining();
        let allowed = admit(len, requested)?;
        if allowed > requested {
            return Err(VfsError::InvalidInput);
        }
        let mut admitted = (&mut buf).take(allowed as u64);
        let written = self.write_at_locked(&mut admitted, len)?;
        let new_end = len
            .checked_add(written as u64)
            .ok_or(VfsError::InvalidInput)?;
        Ok(Some((written, new_end)))
    }

    /// Appends `buf` to the end of the file. Returns `(bytes_written, new_end)`.
    pub fn append(&self, buf: impl Read + IoBuf) -> VfsResult<(usize, u64)> {
        self.append_with_admission(buf, |_offset, requested| Ok(requested))
    }

    /// Appends one scatter list as a single inode append transaction.
    ///
    /// Empty slices are ignored. A short element stops the transaction, and
    /// an error keeps the same propagation semantics as repeated scalar
    /// appends, but no other cached or direct writer can enter between two
    /// nonempty elements.
    pub fn append_vectored(&self, src: &[&[u8]]) -> VfsResult<(usize, u64)> {
        let _direct_guard = self.shared.direct_io_lock.lock();
        let _append_guard = self.shared.append_lock.write();
        let file = self.inner.entry().as_file()?;
        let mut total = 0usize;
        let mut end = file.len()?;

        for buf in src.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let written = match self.write_at_locked(buf, end) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += written;
            end += written as u64;
            if written < requested || written == 0 {
                break;
            }
        }
        Ok((total, end))
    }

    pub(super) fn append_vectored_with_held_native_mutation(
        &self,
        src: &[&[u8]],
        _native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<(usize, u64)> {
        self.append_vectored(src)
    }

    /// Truncates or extends the file to `len` bytes.
    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        let native_mutation = begin_native_location_mutation(&self.inner, false)?;
        self.set_len_with_held_native_mutation(len, held_native_writeback_gate(&native_mutation))
    }

    pub(super) fn set_len_with_held_native_mutation(
        &self,
        len: u64,
        native_gate: HeldNativeWritebackGate<'_>,
    ) -> VfsResult<()> {
        let _direct_guard = self.shared.direct_io_lock.lock();
        let _writeback_guard = self.shared.writeback_lock.write();
        wait_for_all_writeback_clear(&self.shared);
        let file = self.inner.entry().as_file()?;
        let old_len = file.len()?;
        // Keep direct extent requests excluded through the lower operation.
        // Precise cache pins wholly before the changed tail remain valid.
        let changed_page = old_len.min(len) / PAGE_SIZE as u64;
        let mutation = Self::begin_shared_cache_invalidating_range(
            &self.shared,
            changed_page * PAGE_SIZE as u64..u64::MAX,
        )?;
        self.admit_truncate(old_len, len)?;
        let partial_page = (old_len > len && len % PAGE_SIZE as u64 != 0)
            .then_some((len / PAGE_SIZE as u64) as u32);
        let mut discarded = if old_len > len {
            let mut invalidation = CachedPageInvalidationTransaction::new(&mutation);
            // Include the boundary page. A lower filesystem that reports a
            // non-atomic failure may already have changed its tail; on success
            // the retained prefix is restored below with a zeroed suffix.
            invalidation.stage_from(len / PAGE_SIZE as u64)?;
            invalidation.prepare_evictions()?;
            invalidation.writeback_with_held_native_gate(file, true, native_gate)?;
            Some(invalidation)
        } else {
            None
        };
        if let Err(error) = file.set_len(len) {
            if let Some(invalidation) = discarded.take()
                && !file.set_len_failure_is_atomic()
            {
                invalidation.commit_discard();
                release_cached_file_writeback_anchor_if_clean(&self.shared);
            }
            return Err(error);
        }

        let old_last_page = (old_len / PAGE_SIZE as u64) as u32;
        if old_len < len {
            let mut guard = self.shared.page_cache.lock();
            if let Some(page) = guard.get_mut(&old_last_page) {
                let page_start = old_last_page as u64 * PAGE_SIZE as u64;
                let old_page_offset = (old_len - page_start) as usize;
                let new_page_offset = (len - page_start).min(PAGE_SIZE as u64) as usize;
                page.data()[old_page_offset..new_page_offset].fill(0);
            }
        } else if let (Some(partial_page), Some(invalidation)) = (partial_page, discarded.as_mut())
        {
            invalidation.restore_staged_page(partial_page, |page| {
                let page_start = partial_page as u64 * PAGE_SIZE as u64;
                let new_page_offset = len.saturating_sub(page_start).min(PAGE_SIZE as u64) as usize;
                page.data()[new_page_offset..].fill(0);
                // Preserve dirty state for retained bytes; writeback clamps
                // the final partial page to the new inode length.
                page.mark_dirty();
            });
        }
        if let Some(invalidation) = discarded.take() {
            invalidation.commit_discard();
        }
        release_cached_file_writeback_anchor_if_clean(&self.shared);
        retain_cached_file_writeback_anchor_if_dirty(&self.inner, &self.shared);
        Ok(())
    }

    /// Flushes all cached pages back to disk.
    pub fn sync(&self, data_only: bool) -> VfsResult<()> {
        if self.in_memory {
            self.sync_in_memory_cache();
            return Ok(());
        }
        let file = self.inner.entry().as_file()?;
        let native_mutation = begin_file_node_writeback_mutation(file)?;
        let _direct_guard = self.shared.direct_io_lock.lock();
        flush_dirty_cache_shared_with_held_native_gate(
            &self.shared,
            file,
            held_native_writeback_gate(&native_mutation),
        )?;
        self.inner.flush_metadata_time_overlay()?;
        file.sync(data_only)?;
        Ok(())
    }

    /// Returns a reference to the underlying [`Location`].
    pub fn location(&self) -> &Location {
        &self.inner
    }
}
