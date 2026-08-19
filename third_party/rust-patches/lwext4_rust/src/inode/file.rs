use alloc::vec::Vec;
use core::{
    mem::{self, offset_of},
    slice,
};

use super::InodeRef;
use crate::{
    Ext4Error, Ext4Result, InodeType, SystemHal, WritebackGuard,
    error::Context,
    ffi::*,
    hot::{
        ENABLE_EXTENT_STATUS_CACHE, ExtentStatusKind, ExtentStatusRun, record_extent_get_blocks,
        record_legacy_dblk_lookup, record_mapped_overwrite_hit, record_mapped_overwrite_miss,
        record_mapped_read,
    },
    iomap::{
        FIEMAP_EXTENT_LAST, FIEMAP_EXTENT_UNWRITTEN, FIEMAP_MAX_BYTES, FiemapExtent,
        FiemapResult, MappedRun, MappedRunKind,
    },
    util::get_block_size,
};

#[derive(Clone, Copy)]
struct ExtentRun {
    fblock: u64,
    blocks: u32,
}

#[derive(Clone, Copy)]
struct FiemapRun {
    fblock: u64,
    blocks: u32,
    unwritten: bool,
}

const MAX_IOMAP_RUNS: usize = 256;

fn retain_fiemap_extent(
    extent: FiemapExtent,
    total_extents: &mut usize,
    retained: &mut Option<Vec<FiemapExtent>>,
    max_extents: usize,
) -> Ext4Result<()> {
    *total_extents = total_extents
        .checked_add(1)
        .ok_or_else(|| Ext4Error::new(EFBIG as _, "fiemap extent count overflow"))?;
    if let Some(extents) = retained.as_mut()
        && extents.len() < max_extents
    {
        extents
            .try_reserve(1)
            .map_err(|_| Ext4Error::new(ENOMEM as _, "fiemap extent allocation failed"))?;
        extents.push(extent);
    }
    Ok(())
}

impl From<ExtentStatusRun> for ExtentRun {
    fn from(run: ExtentStatusRun) -> Self {
        debug_assert_eq!(run.kind == ExtentStatusKind::Hole, run.pblock == 0);
        Self {
            fblock: run.pblock,
            blocks: run.blocks,
        }
    }
}

fn take<'a>(buf: &mut &'a [u8], cnt: usize) -> &'a [u8] {
    let (first, rem) = buf.split_at(cnt.min(buf.len()));
    *buf = rem;
    first
}
fn take_mut<'a>(buf: &mut &'a mut [u8], cnt: usize) -> &'a mut [u8] {
    // use mem::take to circumvent lifetime issues
    let pos = cnt.min(buf.len());
    let (first, rem) = mem::take(buf).split_at_mut(pos);
    *buf = rem;
    first
}

impl<Hal: SystemHal> InodeRef<Hal> {
    fn get_inode_fblock(&mut self, block: u32) -> Ext4Result<u64> {
        record_legacy_dblk_lookup();
        unsafe {
            let mut fblock = 0u64;
            ext4_fs_get_inode_dblk_idx(self.inner.as_mut(), block, &mut fblock, true)
                .context("ext4_fs_get_inode_dblk_idx")?;
            Ok(fblock)
        }
    }
    fn init_inode_fblock(&mut self, block: u32) -> Ext4Result<u64> {
        record_legacy_dblk_lookup();
        unsafe {
            let mut fblock = 0u64;
            let mut metadata_may_have_changed = false;
            ext4_fs_init_inode_dblk_idx_status(
                self.inner.as_mut(),
                block,
                &mut fblock,
                &mut metadata_may_have_changed,
            )
            .context("ext4_fs_init_inode_dblk_idx")
            .map_err(|error| error.with_metadata_may_have_changed(metadata_may_have_changed))?;
            Ok(fblock)
        }
    }
    fn append_inode_fblock(&mut self) -> Ext4Result<(u64, u32)> {
        record_legacy_dblk_lookup();
        unsafe {
            let mut fblock = 0u64;
            let mut block = 0u32;
            let mut metadata_may_have_changed = false;
            ext4_fs_append_inode_dblk_status(
                self.inner.as_mut(),
                &mut fblock,
                &mut block,
                &mut metadata_may_have_changed,
            )
            .context("ext4_fs_append_inode_dblk")
            .map_err(|error| error.with_metadata_may_have_changed(metadata_may_have_changed))?;
            Ok((fblock, block))
        }
    }

    fn uses_extents(&self) -> bool {
        unsafe { ext4_inode_has_flag(self.inner.inode, EXT4_INODE_FLAG_EXTENTS) }
    }

    fn map_extent_run(
        &mut self,
        block: u32,
        max_blocks: u32,
        create: bool,
    ) -> Ext4Result<Option<ExtentRun>> {
        self.map_extent_run_impl(block, max_blocks, create, true)
    }

    fn map_extent_run_without_cache(
        &mut self,
        block: u32,
        max_blocks: u32,
        create: bool,
    ) -> Ext4Result<Option<ExtentRun>> {
        self.map_extent_run_impl(block, max_blocks, create, false)
    }

    /// Looks up one extent-tree run for FIEMAP without touching the hot
    /// extent-status cache. In particular, an unwritten extent retains its
    /// physical block number and is returned with an explicit state bit rather
    /// than being converted into a cached hole.
    fn map_fiemap_extent_run(
        &mut self,
        block: u32,
        max_blocks: u32,
    ) -> Ext4Result<Option<FiemapRun>> {
        if max_blocks == 0 || !self.uses_extents() {
            return Ok(None);
        }

        unsafe {
            let mut fblock = 0u64;
            let mut blocks = 0u32;
            let mut unwritten = false;
            ext4_extent_get_blocks_fiemap(
                self.inner.as_mut(),
                block,
                max_blocks,
                &mut fblock,
                &mut blocks,
                &mut unwritten,
            )
            .context("ext4_extent_get_blocks_fiemap")?;
            let blocks = blocks.min(max_blocks);
            if blocks == 0 {
                return Ok(None);
            }
            Ok(Some(FiemapRun {
                fblock,
                blocks,
                unwritten: fblock != 0 && unwritten,
            }))
        }
    }

    fn map_extent_run_impl(
        &mut self,
        block: u32,
        max_blocks: u32,
        create: bool,
        populate_extent_status: bool,
    ) -> Ext4Result<Option<ExtentRun>> {
        if max_blocks == 0 || !self.uses_extents() {
            return Ok(None);
        }

        let cache_enabled = populate_extent_status
            && ENABLE_EXTENT_STATUS_CACHE.load(core::sync::atomic::Ordering::Relaxed);
        if cache_enabled && !create {
            if let Some(run) = self.extent_status.lookup(block, max_blocks) {
                return Ok(Some(run.into()));
            }
        } else if cache_enabled {
            self.extent_status.invalidate_range(block, max_blocks);
        }

        unsafe {
            let mut fblock = 0u64;
            let mut blocks = 0u32;
            let mut metadata_may_have_changed = false;
            ext4_extent_get_blocks_status(
                self.inner.as_mut(),
                block,
                max_blocks,
                &mut fblock,
                create,
                &mut blocks,
                &mut metadata_may_have_changed,
            )
            .context("ext4_extent_get_blocks")
            .map_err(|error| error.with_metadata_may_have_changed(metadata_may_have_changed))?;
            let blocks = blocks.min(max_blocks);
            record_extent_get_blocks(max_blocks, blocks, create);
            if blocks == 0 {
                return Ok(None);
            }
            if create && fblock == 0 {
                return Ok(None);
            }
            let kind = if fblock == 0 {
                ExtentStatusKind::Hole
            } else {
                ExtentStatusKind::Written
            };
            if cache_enabled {
                self.extent_status.insert(block, fblock, blocks, kind);
            }
            Ok(Some(ExtentRun { fblock, blocks }))
        }
    }

    fn insert_observed_extent_run(&mut self, block: u32, fblock: u64, blocks: u32) {
        if !ENABLE_EXTENT_STATUS_CACHE.load(core::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let kind = if fblock == 0 {
            ExtentStatusKind::Hole
        } else {
            ExtentStatusKind::Written
        };
        self.extent_status.insert(block, fblock, blocks, kind);
    }

    fn read_mapped_fblock(&mut self, block: u32) -> Ext4Result<u64> {
        if let Some(run) = self.map_extent_run(block, 1, false)? {
            return Ok(run.fblock);
        }
        let fblock = self.get_inode_fblock(block)?;
        self.insert_observed_extent_run(block, fblock, 1);
        Ok(fblock)
    }

    /// Scans the existing ext4 block map for a typed FIEMAP result.
    ///
    /// The scan is deliberately performed while the owning filesystem lock is
    /// held by the caller.  It rounds the requested byte range out to block
    /// boundaries, omits holes, and retains only the requested prefix.  The
    /// complete walk is still performed after the prefix is full so a caller
    /// can distinguish a complete result and so `FIEMAP_EXTENT_LAST` is only
    /// emitted when it is justified.
    pub fn map_extents(
        &mut self,
        start: u64,
        length: u64,
        max_extents: usize,
    ) -> Ext4Result<FiemapResult> {
        if start >= FIEMAP_MAX_BYTES {
            return Err(Ext4Error::new(EFBIG as _, "fiemap start exceeds maxbytes"));
        }
        if length > FIEMAP_MAX_BYTES - start {
            return Err(Ext4Error::new(EFBIG as _, "fiemap range exceeds maxbytes"));
        }
        if max_extents > crate::iomap::MAX_FIEMAP_EXTENTS {
            return Err(Ext4Error::new(
                EINVAL as _,
                "fiemap extent capacity exceeds bounded interface",
            ));
        }

        let file_size = self.size();
        let requested_end = start.checked_add(length).unwrap_or(u64::MAX);
        let end = requested_end.min(file_size);
        if length == 0 || start >= file_size || end <= start {
            return Ok(FiemapResult::new(Vec::new(), 0, true));
        }

        let block_size = u64::from(get_block_size(self.superblock()));
        if block_size == 0 {
            return Err(Ext4Error::new(EINVAL as _, "invalid ext4 block size"));
        }

        // `ext4_extent_get_blocks_status` takes a u32 logical block number;
        // reject an unrepresentable range rather than truncating it.
        let first_block = start / block_size;
        let last_block = (end - 1) / block_size;
        let first_block = u32::try_from(first_block)
            .map_err(|_| Ext4Error::new(EINVAL as _, "fiemap logical block overflow"))?;
        let last_block = u32::try_from(last_block)
            .map_err(|_| Ext4Error::new(EINVAL as _, "fiemap logical block overflow"))?;

        let mut retained = (max_extents != 0).then(Vec::new);
        let mut total_extents = 0usize;
        let mut first_extent = None;
        let mut last_extent = None;
        let mut pending = None;
        let mut block = first_block;
        loop {
            let remaining = last_block
                .checked_sub(block)
                .and_then(|remaining| remaining.checked_add(1))
                .ok_or_else(|| Ext4Error::new(EINVAL as _, "fiemap block range overflow"))?;

            let (pblock, blocks, unwritten) = if self.uses_extents() {
                match self.map_fiemap_extent_run(block, remaining)? {
                    Some(run) if run.blocks != 0 => {
                        (run.fblock, run.blocks.min(remaining), run.unwritten)
                    }
                    _ => (0, 1, false),
                }
            } else {
                // Ext4 normally creates extent-mapped regular files, but
                // legacy indirect inodes remain readable and should not turn
                // into fabricated holes for FIEMAP.
                (self.get_inode_fblock(block)?, 1, false)
            };
            let blocks = blocks.max(1).min(remaining);

            if pblock != 0 {
                let run_logical = u64::from(block)
                    .checked_mul(block_size)
                    .ok_or_else(|| Ext4Error::new(EINVAL as _, "fiemap logical byte overflow"))?;
                let run_end = run_logical
                    .checked_add(u64::from(blocks).checked_mul(block_size).ok_or_else(|| {
                        Ext4Error::new(EINVAL as _, "fiemap run length overflow")
                    })?)
                    .ok_or_else(|| Ext4Error::new(EINVAL as _, "fiemap logical end overflow"))?;
                let logical = start.max(run_logical);
                let logical_end = end.min(run_end);
                if logical >= logical_end {
                    if block == last_block {
                        break;
                    }
                    block = block
                        .checked_add(blocks)
                        .ok_or_else(|| {
                            Ext4Error::new(EINVAL as _, "fiemap block advance overflow")
                        })?;
                    continue;
                }
                let physical = pblock
                    .checked_mul(block_size)
                    .ok_or_else(|| Ext4Error::new(EINVAL as _, "fiemap physical byte overflow"))?;
                let physical = physical
                    .checked_add(logical - run_logical)
                    .ok_or_else(|| Ext4Error::new(EINVAL as _, "fiemap physical offset overflow"))?;
                let bytes = logical_end - logical;

                let flags = if unwritten {
                    FIEMAP_EXTENT_UNWRITTEN
                } else {
                    0
                };
                let current = FiemapExtent::new(logical, physical, bytes, flags);
                let can_extend = pending.as_ref().is_some_and(|previous: &FiemapExtent| {
                    previous.logical.checked_add(previous.length) == Some(current.logical)
                        && previous.physical.checked_add(previous.length)
                            == Some(current.physical)
                        && previous.flags == current.flags
                });
                if can_extend {
                    if let Some(previous) = pending.as_mut() {
                        previous.length = previous
                            .length
                            .checked_add(current.length)
                            .ok_or_else(|| {
                                Ext4Error::new(EFBIG as _, "fiemap extent length overflow")
                            })?;
                    }
                } else {
                    if let Some(previous) = pending.take() {
                        if first_extent.is_none() {
                            first_extent = Some(previous);
                        }
                        last_extent = Some(previous);
                        retain_fiemap_extent(
                            previous,
                            &mut total_extents,
                            &mut retained,
                            max_extents,
                        )?;
                    }
                    pending = Some(current);
                }
            } else if let Some(previous) = pending.take() {
                if first_extent.is_none() {
                    first_extent = Some(previous);
                }
                last_extent = Some(previous);
                retain_fiemap_extent(previous, &mut total_extents, &mut retained, max_extents)?;
            }

            if block == last_block {
                break;
            }
            block = block
                .checked_add(blocks)
                .ok_or_else(|| Ext4Error::new(EINVAL as _, "fiemap block advance overflow"))?;
            if block > last_block {
                break;
            }
        }

        if let Some(previous) = pending.take() {
            if first_extent.is_none() {
                first_extent = Some(previous);
            }
            last_extent = Some(previous);
            retain_fiemap_extent(previous, &mut total_extents, &mut retained, max_extents)?;
        }

        let mapped_extents = if max_extents == 0 {
            u32::try_from(total_extents)
                .map_err(|_| Ext4Error::new(EFBIG as _, "fiemap extent count overflow"))?
        } else {
            u32::try_from(retained.as_ref().map_or(0, Vec::len))
                .map_err(|_| Ext4Error::new(EFBIG as _, "fiemap extent count overflow"))?
        };
        let complete = max_extents == 0 || total_extents <= max_extents;
        if complete && end == file_size && max_extents != 0 {
            if let Some(last) = retained.as_mut().and_then(|extents| extents.last_mut()) {
                last.flags |= FIEMAP_EXTENT_LAST;
            }
        }

        let extents = retained.unwrap_or_else(Vec::new);
        last_extent = extents.last().copied().or(last_extent);
        Ok(FiemapResult::with_bounds(
            extents,
            mapped_extents,
            complete,
            first_extent,
            last_extent,
        ))
    }

    pub(crate) fn map_iomap_runs(
        &mut self,
        pos: u64,
        len: usize,
        overwrite_only: bool,
    ) -> Ext4Result<Option<Vec<MappedRun>>> {
        self.map_iomap_runs_impl(pos, len, overwrite_only, true)
    }

    /// Maps an eligibility range without populating the hot inode or extent
    /// status caches.  Physical-I/O prepare must be observational for a
    /// rejected hole/unwritten/EOF request so the caller can synchronously
    /// choose its fallback with zero cache side effects.
    pub(crate) fn map_iomap_runs_without_cache(
        &mut self,
        pos: u64,
        len: usize,
        overwrite_only: bool,
    ) -> Ext4Result<Option<Vec<MappedRun>>> {
        self.map_iomap_runs_impl(pos, len, overwrite_only, false)
    }

    fn map_iomap_runs_impl(
        &mut self,
        pos: u64,
        len: usize,
        overwrite_only: bool,
        populate_extent_status: bool,
    ) -> Ext4Result<Option<Vec<MappedRun>>> {
        let block_size = get_block_size(self.superblock());
        if len == 0
            || block_size == 0
            || pos % block_size as u64 != 0
            || len % block_size as usize != 0
        {
            return Ok(None);
        }
        if overwrite_only && pos + len as u64 > self.size() {
            return Ok(None);
        }

        let mut runs = Vec::new();
        let mut block = (pos / block_size as u64) as u32;
        let Some(block_count) = u32::try_from(len / block_size as usize).ok() else {
            return Ok(None);
        };
        let Some(block_end) = block.checked_add(block_count) else {
            return Ok(None);
        };
        let mut file_offset = pos;
        while block < block_end {
            let remaining = block_end - block;
            let run = if populate_extent_status {
                self.map_extent_run(block, remaining, false)?
            } else {
                self.map_extent_run_without_cache(block, remaining, false)?
            };
            let (fblock, blocks) = if let Some(run) = run {
                (run.fblock, run.blocks)
            } else if overwrite_only || !self.uses_extents() {
                return Ok(None);
            } else {
                let fblock = self.get_inode_fblock(block)?;
                if populate_extent_status {
                    self.insert_observed_extent_run(block, fblock, 1);
                }
                (fblock, 1)
            };
            let blocks = blocks.min(remaining);
            if blocks == 0 {
                return Ok(None);
            }
            let kind = if fblock == 0 {
                if overwrite_only {
                    return Ok(None);
                }
                MappedRunKind::Hole
            } else {
                MappedRunKind::Written
            };
            if runs.len() == MAX_IOMAP_RUNS {
                return Ok(None);
            }
            runs.push(MappedRun {
                file_offset,
                pblock: fblock,
                bytes: blocks as usize * block_size as usize,
                kind,
                seq: self.mapping_seq,
            });
            block += blocks;
            let Some(next_offset) = file_offset.checked_add(blocks as u64 * block_size as u64)
            else {
                return Ok(None);
            };
            file_offset = next_offset;
        }
        Ok(Some(runs))
    }

    /// Maps one exact, contiguous logical range without allocating a run
    /// vector. Physical direct I/O only accepts a single written extent; the
    /// caller captures the mapping sequence while holding the filesystem lock
    /// and submits the device request only after releasing it.
    pub(crate) fn map_iomap_run(
        &mut self,
        pos: u64,
        len: usize,
        overwrite_only: bool,
    ) -> Ext4Result<Option<MappedRun>> {
        let block_size = get_block_size(self.superblock());
        if len == 0
            || block_size == 0
            || pos % block_size as u64 != 0
            || len % block_size as usize != 0
            || (overwrite_only
                && pos
                    .checked_add(len as u64)
                    .is_none_or(|end| end > self.size()))
        {
            return Ok(None);
        }
        let blocks = u32::try_from(len / block_size as usize)
            .map_err(|_| Ext4Error::new(EINVAL as _, "mapped physical I/O block count overflow"))?;
        let block = u32::try_from(pos / block_size as u64).map_err(|_| {
            Ext4Error::new(EINVAL as _, "mapped physical I/O block offset overflow")
        })?;
        let Some(run) = self.map_extent_run(block, blocks, false)? else {
            return Ok(None);
        };
        if run.blocks != blocks {
            return Ok(None);
        }
        let kind = if run.fblock == 0 {
            if overwrite_only {
                return Ok(None);
            }
            MappedRunKind::Hole
        } else {
            MappedRunKind::Written
        };
        Ok(Some(MappedRun {
            file_offset: pos,
            pblock: run.fblock,
            bytes: len,
            kind,
            seq: self.mapping_seq,
        }))
    }

    fn ensure_mapped_runs_current(&self, runs: &[MappedRun]) -> Ext4Result<()> {
        let mut expected_offset = runs.first().map(|run| run.file_offset).unwrap_or(0);
        for run in runs {
            if run.seq != self.mapping_seq || run.file_offset != expected_offset {
                return Err(Ext4Error::new(EIO as _, "stale mapped run"));
            }
            expected_offset = run.end_offset();
        }
        Ok(())
    }

    fn read_mapped_runs(
        &self,
        bdev: *mut ext4_blockdev,
        mut buf: &mut [u8],
        runs: &[MappedRun],
        block_size: u32,
    ) -> Ext4Result<()> {
        self.ensure_mapped_runs_current(runs)?;
        for run in runs {
            let chunk = take_mut(&mut buf, run.bytes);
            match run.kind {
                MappedRunKind::Written => {
                    unsafe {
                        ext4_blocks_get_direct(
                            bdev,
                            chunk.as_mut_ptr() as _,
                            run.pblock,
                            (run.bytes / block_size as usize) as u32,
                        )
                    }
                    .context("ext4_blocks_get_direct")?;
                }
                MappedRunKind::Hole => chunk.fill(0),
            }
        }
        record_mapped_read(runs.len(), runs.iter().map(|run| run.bytes).sum());
        Ok(())
    }

    fn write_mapped_overwrite_runs(
        &self,
        bdev: *mut ext4_blockdev,
        mut buf: &[u8],
        runs: &[MappedRun],
        block_size: u32,
    ) -> Ext4Result<usize> {
        self.ensure_mapped_runs_current(runs)?;
        let mut written = 0;
        for run in runs {
            if run.kind != MappedRunKind::Written || run.pblock == 0 {
                return Err(Ext4Error::new(EIO as _, "invalid overwrite mapped run"));
            }
            let chunk = take(&mut buf, run.bytes);
            unsafe {
                ext4_blocks_set_direct(
                    bdev,
                    chunk.as_ptr() as _,
                    run.pblock,
                    (run.bytes / block_size as usize) as u32,
                )
            }
            .context("ext4_blocks_set_direct")?;
            written += chunk.len();
        }
        record_mapped_overwrite_hit(written);
        Ok(written)
    }

    fn read_bytes(&mut self, offset: u64, buf: &mut [u8]) -> Ext4Result<()> {
        unsafe {
            let bdev = (*self.inner.fs).bdev;
            ext4_block_readbytes(bdev, offset, buf.as_mut_ptr() as _, buf.len() as _)
                .context("ext4_block_readbytes")
        }
    }
    fn write_bytes(&mut self, offset: u64, buf: &[u8]) -> Ext4Result<()> {
        unsafe {
            let bdev = (*self.inner.fs).bdev;
            ext4_block_writebytes(bdev, offset, buf.as_ptr() as _, buf.len() as _)
                .context("ext4_block_writebytes")
        }
    }

    pub fn read_at(&mut self, mut buf: &mut [u8], pos: u64) -> Ext4Result<usize> {
        unsafe {
            let file_size = self.size();
            let block_size = get_block_size(self.superblock());
            let bdev = (*self.inner.fs).bdev;

            if pos >= file_size || buf.is_empty() {
                return Ok(0);
            }
            let to_be_read = buf.len().min((file_size - pos) as usize);
            buf = &mut buf[..to_be_read];

            let inode = self.raw_inode();

            // symlink inline data
            if self.inode_type() == InodeType::Symlink && file_size < size_of::<[u32; 15]>() as u64
            {
                let content = (inode as *const _ as *const u8).add(offset_of!(ext4_inode, blocks));
                let buf = take_mut(&mut buf, (file_size - pos) as usize);
                buf.copy_from_slice(slice::from_raw_parts(content.add(pos as usize), buf.len()));
            }

            let mut block_start = (pos / block_size as u64) as u32;
            // This is inclusive!
            let block_end = ((pos + buf.len() as u64).min(file_size) / block_size as u64) as u32;

            let offset = pos % block_size as u64;
            if offset > 0 {
                let buf = take_mut(&mut buf, block_size as usize - offset as usize);
                let fblock = self.read_mapped_fblock(block_start)?;
                if fblock != 0 {
                    self.read_bytes(fblock * block_size as u64 + offset, buf)?;
                } else {
                    buf.fill(0);
                }
                block_start += 1;
            }

            let guard = WritebackGuard::new(bdev);

            // Each block corresponds to a fblock, and we can read multiple
            // fblocks at once if they are consecutive.
            let mut fblock_start = 0;
            let mut fblock_count = 0;

            let flush_fblock_segment = |buf: &mut &mut [u8], start: u64, count: u32| {
                if count == 0 {
                    return Ok(());
                }
                let buf = take_mut(buf, count as usize * block_size as usize);
                ext4_blocks_get_direct(bdev, buf.as_mut_ptr() as _, start, count)
                    .context("ext4_blocks_get_direct")
            };
            let mut block = block_start;
            while block < block_end {
                let run = self.map_extent_run(block, block_end - block, false)?;
                let (fblock, blocks) = if let Some(run) = run {
                    (run.fblock, run.blocks)
                } else {
                    let fblock = self.get_inode_fblock(block)?;
                    self.insert_observed_extent_run(block, fblock, 1);
                    (fblock, 1)
                };
                if fblock != fblock_start + fblock_count as u64 {
                    flush_fblock_segment(&mut buf, fblock_start, fblock_count)?;
                    fblock_start = fblock;
                    fblock_count = 0;
                }

                if fblock == 0 {
                    flush_fblock_segment(&mut buf, fblock_start, fblock_count)?;
                    fblock_start = 0;
                    fblock_count = 0;
                    take_mut(&mut buf, blocks as usize * block_size as usize).fill(0);
                } else {
                    fblock_count += blocks;
                }
                block += blocks;
            }
            flush_fblock_segment(&mut buf, fblock_start, fblock_count)?;

            drop(guard);

            if buf.len() >= block_size as usize {
                return Err(Ext4Error::new(
                    EIO as _,
                    "ext4 read mapping left an invalid trailing segment",
                ));
            }
            if !buf.is_empty() {
                let fblock = self.read_mapped_fblock(block_end)?;
                if fblock != 0 {
                    self.read_bytes(fblock * block_size as u64, buf)?;
                } else {
                    buf.fill(0);
                }
            }

            Ok(to_be_read)
        }
    }

    pub fn read_at_aligned_hot(&mut self, mut buf: &mut [u8], pos: u64) -> Ext4Result<usize> {
        unsafe {
            let file_size = self.size();
            let block_size = get_block_size(self.superblock());
            let bdev = (*self.inner.fs).bdev;

            if pos >= file_size || buf.is_empty() {
                return Ok(0);
            }
            if block_size == 0
                || block_size != 4096
                || pos % block_size as u64 != 0
                || buf.len() % block_size as usize != 0
                || (self.inode_type() == InodeType::Symlink
                    && file_size < size_of::<[u32; 15]>() as u64)
            {
                return self.read_at(buf, pos);
            }

            let to_be_read = buf.len().min((file_size - pos) as usize);
            if to_be_read % block_size as usize != 0 {
                return self.read_at(buf, pos);
            }
            buf = &mut buf[..to_be_read];

            let block_start = (pos / block_size as u64) as u32;
            let block_end = block_start + (to_be_read / block_size as usize) as u32;

            let guard = WritebackGuard::new(bdev);
            if let Some(runs) = self.map_iomap_runs(pos, to_be_read, false)? {
                self.read_mapped_runs(bdev, buf, &runs, block_size)?;
            } else {
                let mut fblock_start = 0;
                let mut fblock_count = 0;

                let flush_fblock_segment = |buf: &mut &mut [u8], start: u64, count: u32| {
                    if count == 0 {
                        return Ok(());
                    }
                    let buf = take_mut(buf, count as usize * block_size as usize);
                    ext4_blocks_get_direct(bdev, buf.as_mut_ptr() as _, start, count)
                        .context("ext4_blocks_get_direct")
                };
                let mut block = block_start;
                while block < block_end {
                    let run = self.map_extent_run(block, block_end - block, false)?;
                    let (fblock, blocks) = if let Some(run) = run {
                        (run.fblock, run.blocks)
                    } else {
                        let fblock = self.get_inode_fblock(block)?;
                        self.insert_observed_extent_run(block, fblock, 1);
                        (fblock, 1)
                    };
                    if fblock != fblock_start + fblock_count as u64 {
                        flush_fblock_segment(&mut buf, fblock_start, fblock_count)?;
                        fblock_start = fblock;
                        fblock_count = 0;
                    }

                    if fblock == 0 {
                        flush_fblock_segment(&mut buf, fblock_start, fblock_count)?;
                        fblock_start = 0;
                        fblock_count = 0;
                        take_mut(&mut buf, blocks as usize * block_size as usize).fill(0);
                    } else {
                        fblock_count += blocks;
                    }
                    block += blocks;
                }
                flush_fblock_segment(&mut buf, fblock_start, fblock_count)?;
            }
            drop(guard);

            Ok(to_be_read)
        }
    }

    pub fn write_at(&mut self, mut buf: &[u8], pos: u64) -> Ext4Result<usize> {
        unsafe {
            let mut file_size = self.size();
            if pos > file_size {
                self.set_len(pos)?;
                // If we extend the file, we need to update the file size.
                file_size = self.size();
            }
            self.invalidate_mapping_seq();

            let block_size = get_block_size(self.superblock());
            let block_count = file_size.div_ceil(block_size as u64) as u32;
            let bdev = (*self.inner.fs).bdev;

            if buf.is_empty() {
                return Ok(0);
            }
            let to_be_written = buf.len();
            let touched_start = (pos / block_size as u64) as u32;
            let touched_end = (pos + buf.len() as u64).div_ceil(block_size as u64) as u32;
            if ENABLE_EXTENT_STATUS_CACHE.load(core::sync::atomic::Ordering::Relaxed) {
                self.extent_status
                    .invalidate_range(touched_start, touched_end.saturating_sub(touched_start));
            }

            // TODO: symlink?

            let get_fblock = |this: &mut Self, block: u32| -> Ext4Result<u64> {
                if block < block_count {
                    this.init_inode_fblock(block)
                } else {
                    let (fblock, new_block) = this.append_inode_fblock()?;
                    if block != new_block {
                        return Err(Ext4Error::new(
                            EIO as _,
                            "ext4 append returned an unexpected logical block",
                        )
                        .with_metadata_may_have_changed(true));
                    }
                    Ok(fblock)
                }
            };

            let mut block_start = (pos / block_size as u64) as u32;
            // This is inclusive!
            let block_end = ((pos + buf.len() as u64) / block_size as u64) as u32;

            let offset = pos % block_size as u64;
            if offset > 0 {
                let buf = take(&mut buf, block_size as usize - offset as usize);
                let fblock = if let Some(run) = self.map_extent_run(block_start, 1, true)? {
                    run.fblock
                } else {
                    let fblock = get_fblock(self, block_start)?;
                    self.insert_observed_extent_run(block_start, fblock, 1);
                    fblock
                };
                self.write_bytes(fblock * block_size as u64 + offset, buf)?;
                block_start += 1;
            }

            let mut fblock_start = 0;
            let mut fblock_count = 0;

            let flush_fblock_segment = |buf: &mut &[u8], start: u64, count: u32| {
                if count == 0 {
                    return Ok(());
                }
                let buf = take(buf, count as usize * block_size as usize);
                ext4_blocks_set_direct(bdev, buf.as_ptr() as _, start, count)
                    .context("ext4_blocks_set_direct")
            };
            let mut block = block_start;
            while block < block_end {
                let remaining_blocks = block_end - block;
                let run = self.map_extent_run(block, remaining_blocks, true)?;
                let (fblock, blocks) = if let Some(run) = run {
                    (run.fblock, run.blocks)
                } else {
                    let fblock = get_fblock(self, block)?;
                    self.insert_observed_extent_run(block, fblock, 1);
                    (fblock, 1)
                };
                if fblock != fblock_start + fblock_count as u64 {
                    flush_fblock_segment(&mut buf, fblock_start, fblock_count)?;
                    fblock_start = fblock;
                    fblock_count = 0;
                }
                fblock_count += blocks;
                block += blocks;
            }
            flush_fblock_segment(&mut buf, fblock_start, fblock_count)?;

            if buf.len() >= block_size as usize {
                return Err(Ext4Error::new(
                    EIO as _,
                    "ext4 write mapping left an invalid trailing segment",
                )
                .with_metadata_may_have_changed(true));
            }
            if !buf.is_empty() {
                let fblock = if let Some(run) = self.map_extent_run(block_end, 1, true)? {
                    run.fblock
                } else {
                    let fblock = get_fblock(self, block_end)?;
                    self.insert_observed_extent_run(block_end, fblock, 1);
                    fblock
                };
                self.write_bytes(fblock * block_size as u64, buf)?;
            }

            let end = pos + to_be_written as u64;
            if end > file_size {
                ext4_inode_set_size(self.inner.inode, end);
                self.mark_dirty();
            }

            Ok(to_be_written)
        }
    }

    pub fn write_at_aligned_hot(&mut self, mut buf: &[u8], pos: u64) -> Ext4Result<usize> {
        unsafe {
            let block_size = get_block_size(self.superblock());
            if buf.is_empty() {
                return Ok(0);
            }
            if block_size == 0
                || block_size != 4096
                || pos % block_size as u64 != 0
                || buf.len() % block_size as usize != 0
                || self.inode_type() == InodeType::Symlink
            {
                return self.write_at(buf, pos);
            }

            let mut file_size = self.size();
            if pos > file_size {
                self.set_len(pos)?;
                file_size = self.size();
            }

            let block_count = file_size.div_ceil(block_size as u64) as u32;
            let bdev = (*self.inner.fs).bdev;
            let to_be_written = buf.len();
            if pos + to_be_written as u64 <= file_size {
                if let Some(runs) = self.map_iomap_runs(pos, to_be_written, true)? {
                    return self.write_mapped_overwrite_runs(bdev, buf, &runs, block_size);
                }
                record_mapped_overwrite_miss();
            }

            self.invalidate_mapping_seq();
            let touched_start = (pos / block_size as u64) as u32;
            let touched_blocks = (buf.len() / block_size as usize) as u32;
            if ENABLE_EXTENT_STATUS_CACHE.load(core::sync::atomic::Ordering::Relaxed) {
                self.extent_status
                    .invalidate_range(touched_start, touched_blocks);
            }

            let get_fblock = |this: &mut Self, block: u32| -> Ext4Result<u64> {
                if block < block_count {
                    this.init_inode_fblock(block)
                } else {
                    let (fblock, new_block) = this.append_inode_fblock()?;
                    if block != new_block {
                        return Err(Ext4Error::new(
                            EIO as _,
                            "ext4 append returned an unexpected logical block",
                        )
                        .with_metadata_may_have_changed(true));
                    }
                    Ok(fblock)
                }
            };

            let block_start = (pos / block_size as u64) as u32;
            let block_end = block_start + touched_blocks;
            let mut fblock_start = 0;
            let mut fblock_count = 0;

            let flush_fblock_segment = |buf: &mut &[u8], start: u64, count: u32| {
                if count == 0 {
                    return Ok(());
                }
                let buf = take(buf, count as usize * block_size as usize);
                ext4_blocks_set_direct(bdev, buf.as_ptr() as _, start, count)
                    .context("ext4_blocks_set_direct")
            };
            let mut block = block_start;
            while block < block_end {
                let remaining_blocks = block_end - block;
                let run = self.map_extent_run(block, remaining_blocks, true)?;
                let (fblock, blocks) = if let Some(run) = run {
                    (run.fblock, run.blocks)
                } else {
                    let fblock = get_fblock(self, block)?;
                    self.insert_observed_extent_run(block, fblock, 1);
                    (fblock, 1)
                };
                if fblock != fblock_start + fblock_count as u64 {
                    flush_fblock_segment(&mut buf, fblock_start, fblock_count)?;
                    fblock_start = fblock;
                    fblock_count = 0;
                }
                fblock_count += blocks;
                block += blocks;
            }
            flush_fblock_segment(&mut buf, fblock_start, fblock_count)?;

            let end = pos + to_be_written as u64;
            if end > file_size {
                ext4_inode_set_size(self.inner.inode, end);
                self.mark_dirty();
            }

            Ok(to_be_written)
        }
    }

    pub fn truncate(&mut self, size: u64) -> Ext4Result<()> {
        unsafe {
            let bdev = (*self.inner.fs).bdev;
            let _guard = WritebackGuard::new(bdev);
            let block_size = get_block_size(self.superblock());
            self.invalidate_mapping_seq();
            if ENABLE_EXTENT_STATUS_CACHE.load(core::sync::atomic::Ordering::Relaxed) {
                self.extent_status
                    .invalidate_from((size / block_size as u64) as u32);
            }
            ext4_fs_truncate_inode(self.inner.as_mut(), size).context("ext4_fs_truncate_inode")
        }
    }

    pub(crate) fn initialize_symlink(&mut self, target: &[u8]) -> Ext4Result<()> {
        self.invalidate_mapping_seq();
        if ENABLE_EXTENT_STATUS_CACHE.load(core::sync::atomic::Ordering::Relaxed) {
            self.extent_status.clear();
        }
        let block_size = get_block_size(self.superblock());
        if target.len() > block_size as usize {
            // ENAMETOOLONG
            return 36.context("symlink too long");
        }

        unsafe {
            if target.len() < size_of::<u32>() * EXT4_INODE_BLOCKS as usize {
                let ptr = (self.inner.inode as *mut u8).add(offset_of!(ext4_inode, blocks));
                slice::from_raw_parts_mut(ptr, target.len()).copy_from_slice(target);
                ext4_inode_clear_flag(self.inner.inode, EXT4_INODE_FLAG_EXTENTS);
            } else {
                ext4_fs_inode_blocks_init(self.inner.fs, self.inner.as_mut());
                let mut fblock: u64 = 0;
                let mut sblock: u32 = 0;
                let mut metadata_may_have_changed = false;
                ext4_fs_append_inode_dblk_status(
                    self.inner.as_mut(),
                    &mut fblock,
                    &mut sblock,
                    &mut metadata_may_have_changed,
                )
                .context("ext4_fs_append_inode_dblk")
                .map_err(|error| error.with_metadata_may_have_changed(metadata_may_have_changed))?;

                // Publish the allocated extent in the inode before the data
                // write. If the write fails, the unpublished-inode rollback
                // path can truncate back to zero without leaking this block.
                ext4_inode_set_size(self.inner.inode, target.len() as u64);
                self.mark_dirty();
                let off = fblock * block_size as u64;
                self.write_bytes(off, target)?;
            }
            ext4_inode_set_size(self.inner.inode, target.len() as u64);
            self.mark_dirty();
        }

        Ok(())
    }

    pub fn set_len(&mut self, len: u64) -> Ext4Result<()> {
        static EMPTY: [u8; 4096] = [0; 4096];

        let cur_len = self.size();
        if len != cur_len && ENABLE_EXTENT_STATUS_CACHE.load(core::sync::atomic::Ordering::Relaxed)
        {
            self.extent_status.clear();
        }
        if len != cur_len {
            self.invalidate_mapping_seq();
        }
        if len < cur_len {
            self.truncate(len)?;
        } else if len > cur_len {
            let block_size = get_block_size(self.superblock());
            let old_block_offset = (cur_len % block_size as u64) as usize;
            if old_block_offset > 0 {
                let old_last_block = (cur_len / block_size as u64) as u32;
                let zero_end = len.min((old_last_block as u64 + 1) * block_size as u64);
                let length = (zero_end - cur_len) as usize;
                if length > 0 {
                    let fblock = self.get_inode_fblock(old_last_block)?;
                    if fblock != 0 {
                        self.write_bytes(
                            fblock * block_size as u64 + old_block_offset as u64,
                            &EMPTY[..length],
                        )?;
                    }
                }
            }

            unsafe {
                ext4_inode_set_size(self.inner.inode, len);
            }
            self.mark_dirty();
        }
        Ok(())
    }
}
