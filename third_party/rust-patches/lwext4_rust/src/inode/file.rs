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
    iomap::{MappedRun, MappedRunKind},
    util::get_block_size,
};

#[derive(Clone, Copy)]
struct ExtentRun {
    fblock: u64,
    blocks: u32,
}

const MAX_IOMAP_RUNS: usize = 256;

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
        if max_blocks == 0 || !self.uses_extents() {
            return Ok(None);
        }

        let cache_enabled = ENABLE_EXTENT_STATUS_CACHE.load(core::sync::atomic::Ordering::Relaxed);
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

    pub(crate) fn map_iomap_runs(
        &mut self,
        pos: u64,
        len: usize,
        overwrite_only: bool,
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
        let block_end = block + (len / block_size as usize) as u32;
        let mut file_offset = pos;
        while block < block_end {
            let remaining = block_end - block;
            let run = self.map_extent_run(block, remaining, false)?;
            let (fblock, blocks) = if let Some(run) = run {
                (run.fblock, run.blocks)
            } else if overwrite_only || !self.uses_extents() {
                return Ok(None);
            } else {
                let fblock = self.get_inode_fblock(block)?;
                self.insert_observed_extent_run(block, fblock, 1);
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
            file_offset += blocks as u64 * block_size as u64;
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
