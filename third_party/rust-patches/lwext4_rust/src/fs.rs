use alloc::{boxed::Box, vec::Vec};
use core::{marker::PhantomData, mem, time::Duration};

use crate::{
    DirLookupResult, DirReader, Ext4Error, Ext4Result, FileAttr, InodeRef, InodeType,
    blockdev::{
        AsyncReadSubmission, AsyncWriteSubmission, BlockDevice, EXT4_DEV_BSIZE, Ext4BlockDevice,
    },
    error::Context,
    ffi::*,
    hot::{
        ENABLE_HOT_INODE_CACHE, HotInodeCache, async_mapped_read_enabled, record_async_mapped_read,
        record_async_mapped_read_cookie_reject, record_async_mapped_read_fallback,
        record_hot_inode_hit, record_hot_inode_miss, record_inode_ref_get,
        record_mapped_overwrite_vectored_hit, record_mapped_read, record_mapped_read_vectored,
    },
    iomap::{MappedRun, MappedRunKind},
    util::get_block_size,
};

pub trait SystemHal {
    fn now() -> Option<Duration>;
}

pub struct DummyHal;
impl SystemHal for DummyHal {
    fn now() -> Option<Duration> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct FsConfig {
    pub bcache_size: u32,
}
impl Default for FsConfig {
    fn default() -> Self {
        Self {
            bcache_size: CONFIG_BLOCK_DEV_CACHE_SIZE,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatFs {
    pub inodes_count: u32,
    pub free_inodes_count: u32,

    pub blocks_count: u64,
    pub free_blocks_count: u64,
    pub block_size: u32,
}

fn inode_needs_truncate_on_unlink(ty: InodeType) -> bool {
    matches!(
        ty,
        InodeType::RegularFile | InodeType::Directory | InodeType::Symlink
    )
}

fn runs_align_segments(runs: &[MappedRun], segments: impl IntoIterator<Item = usize>) -> bool {
    let mut run_index = 0usize;
    let mut run_remaining = runs.first().map(|run| run.bytes).unwrap_or(0);
    for segment_len in segments {
        if segment_len == 0 {
            continue;
        }
        if run_index >= runs.len() || segment_len > run_remaining {
            return false;
        }
        run_remaining -= segment_len;
        if run_remaining == 0 {
            run_index += 1;
            run_remaining = runs.get(run_index).map(|run| run.bytes).unwrap_or(0);
        }
    }
    run_index == runs.len() && run_remaining == 0
}

fn segments_are_device_block_sized(segments: impl IntoIterator<Item = usize>) -> bool {
    segments
        .into_iter()
        .all(|len| len == 0 || len % EXT4_DEV_BSIZE == 0)
}

pub struct Ext4Filesystem<Hal: SystemHal, Dev: BlockDevice> {
    inner: Box<ext4_fs>,
    bdev: Ext4BlockDevice<Dev>,
    hot_inodes: HotInodeCache<Hal>,
    _phantom: PhantomData<Hal>,
}

impl<Hal: SystemHal, Dev: BlockDevice> Ext4Filesystem<Hal, Dev> {
    pub fn new(dev: Dev, config: FsConfig) -> Ext4Result<Self> {
        let mut bdev = Ext4BlockDevice::new(dev)?;
        let mut fs = Box::new(unsafe { mem::zeroed() });
        unsafe {
            let bd = bdev.inner.as_mut();
            ext4_fs_init(&mut *fs, bd, false).context("ext4_fs_init")?;

            let bs = get_block_size(&fs.sb);
            ext4_block_set_lb_size(bd, bs);
            ext4_bcache_init_dynamic(bd.bc, config.bcache_size, bs)
                .context("ext4_bcache_init_dynamic")?;
            if bs != (*bd.bc).itemsize {
                return Err(Ext4Error::new(ENOTSUP as _, "block size mismatch"));
            }

            bd.fs = &mut *fs;

            let mut result = Self {
                inner: fs,
                bdev,
                hot_inodes: HotInodeCache::new(),
                _phantom: PhantomData,
            };
            let bd = result.bdev.inner.as_mut();
            ext4_block_bind_bcache(bd, bd.bc).context("ext4_block_bind_bcache")?;
            Ok(result)
        }
    }

    fn inode_ref(&mut self, ino: u32) -> Ext4Result<InodeRef<Hal>> {
        record_inode_ref_get();
        unsafe {
            let mut result = InodeRef::new(mem::zeroed());
            ext4_fs_get_inode_ref(self.inner.as_mut(), ino, result.inner.as_mut())
                .context("ext4_fs_get_inode_ref")?;
            Ok(result)
        }
    }

    fn with_cached_inode_ref<R>(
        &mut self,
        ino: u32,
        f: impl FnOnce(&mut InodeRef<Hal>) -> Ext4Result<R>,
    ) -> Ext4Result<R> {
        if !ENABLE_HOT_INODE_CACHE.load(core::sync::atomic::Ordering::Relaxed) {
            return self.with_inode_ref(ino, f);
        }

        let mut inode = if let Some(inode) = self.hot_inodes.take(ino) {
            record_hot_inode_hit();
            inode
        } else {
            record_hot_inode_miss();
            self.inode_ref(ino)?
        };
        let result = f(&mut inode);
        self.hot_inodes.put(ino, inode);
        result
    }

    fn invalidate_hot_inode(&mut self, ino: u32) {
        self.hot_inodes.invalidate(ino);
    }

    fn drain_hot_inodes(&mut self) {
        self.hot_inodes.drain_all();
    }

    fn clone_ref(&mut self, inode: &InodeRef<Hal>) -> InodeRef<Hal> {
        self.inode_ref(inode.ino()).expect("inode ref clone failed")
    }

    pub fn with_inode_ref<R>(
        &mut self,
        ino: u32,
        f: impl FnOnce(&mut InodeRef<Hal>) -> Ext4Result<R>,
    ) -> Ext4Result<R> {
        let mut inode = self.inode_ref(ino)?;
        f(&mut inode)
    }

    fn mapped_aligned_read_plan(
        &mut self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Ext4Result<Option<(usize, u32, Vec<MappedRun>)>> {
        self.with_cached_inode_ref(ino, |inode| {
            let file_size = inode.size();
            let block_size = get_block_size(inode.superblock());
            if offset >= file_size {
                return Ok(Some((0, block_size, Vec::new())));
            }
            if block_size != 4096
                || offset % block_size as u64 != 0
                || len % block_size as usize != 0
                || inode.inode_type() == InodeType::Symlink
            {
                return Ok(None);
            }
            let to_be_read = len.min((file_size - offset) as usize);
            if to_be_read != len || to_be_read % block_size as usize != 0 {
                return Ok(None);
            }
            let Some(runs) = inode.map_iomap_runs(offset, to_be_read, false)? else {
                return Ok(None);
            };
            if runs
                .iter()
                .any(|run| run.kind != MappedRunKind::Written || run.pblock == 0)
            {
                return Ok(None);
            }
            Ok(Some((to_be_read, block_size, runs)))
        })
    }

    fn mapped_runs_current(&mut self, ino: u32, runs: &[MappedRun]) -> Ext4Result<bool> {
        self.with_cached_inode_ref(ino, |inode| {
            let mut expected_offset = runs.first().map(|run| run.file_offset).unwrap_or(0);
            for run in runs {
                if run.seq != inode.mapping_seq || run.file_offset != expected_offset {
                    return Ok(false);
                }
                expected_offset = run.end_offset();
            }
            Ok(true)
        })
    }

    fn try_read_mapped_runs_async(
        &mut self,
        ino: u32,
        runs: &[MappedRun],
        block_size: u32,
        bufs: &mut [&mut [u8]],
        bytes: usize,
    ) -> Ext4Result<Option<usize>> {
        if !async_mapped_read_enabled() {
            return Ok(None);
        }
        if runs.is_empty() {
            return Ok(Some(0));
        }
        let block_size = block_size as usize;
        if block_size == 0
            || bufs
                .iter()
                .any(|buf| !buf.is_empty() && buf.len() % block_size != 0)
        {
            return Ok(None);
        }
        if !self.mapped_runs_current(ino, runs)? {
            record_async_mapped_read_cookie_reject();
            return Ok(None);
        }

        let mut segment = 0usize;
        let mut submit_batches = 0usize;
        for run in runs {
            let start = segment;
            let mut remaining = run.bytes;
            while remaining > 0 {
                if segment >= bufs.len() {
                    return Ok(None);
                }
                let segment_len = bufs[segment].len();
                segment += 1;
                if segment_len == 0 {
                    continue;
                }
                if segment_len > remaining {
                    return Ok(None);
                }
                remaining -= segment_len;
            }
            let block_id = self.bdev.direct_physical_block_id(run.pblock);
            let Some(stats) = self
                .bdev
                .dev_mut()
                .try_read_blocks_vectored_async(block_id, &mut bufs[start..segment])?
            else {
                return Ok(None);
            };
            submit_batches += stats.submit_batches;
        }

        record_async_mapped_read(runs.len(), bytes, submit_batches);
        Ok(Some(bytes))
    }

    fn try_read_at_aligned_hot_async(
        &mut self,
        ino: u32,
        buf: &mut [u8],
        offset: u64,
    ) -> Ext4Result<Option<usize>> {
        if !async_mapped_read_enabled() {
            return Ok(None);
        }
        let Some((to_be_read, block_size, runs)) =
            self.mapped_aligned_read_plan(ino, offset, buf.len())?
        else {
            record_async_mapped_read_fallback();
            return Ok(None);
        };
        if to_be_read == 0 {
            return Ok(Some(0));
        }
        if !runs_align_segments(&runs, [to_be_read]) {
            record_async_mapped_read_fallback();
            return Ok(None);
        }

        let read_buf = &mut buf[..to_be_read];
        let mut bufs = [read_buf];
        let Some(read) =
            self.try_read_mapped_runs_async(ino, &runs, block_size, &mut bufs, to_be_read)?
        else {
            record_async_mapped_read_fallback();
            return Ok(None);
        };
        record_mapped_read(runs.len(), read);
        Ok(Some(read))
    }

    fn try_read_mapped_runs_async_submit(
        &mut self,
        ino: u32,
        runs: &[MappedRun],
        block_size: u32,
        bufs: &mut [&mut [u8]],
        bytes: usize,
    ) -> Ext4Result<Option<AsyncReadSubmission>> {
        if !async_mapped_read_enabled() {
            return Ok(None);
        }
        if runs.is_empty() {
            return Ok(Some(AsyncReadSubmission::default()));
        }
        let block_size = block_size as usize;
        if block_size == 0
            || bufs
                .iter()
                .any(|buf| !buf.is_empty() && buf.len() % block_size != 0)
        {
            return Ok(None);
        }
        if !self.mapped_runs_current(ino, runs)? {
            record_async_mapped_read_cookie_reject();
            return Ok(None);
        }

        let mut segment = 0usize;
        let mut submission = AsyncReadSubmission {
            bytes,
            ..AsyncReadSubmission::default()
        };
        for run in runs {
            let start = segment;
            let mut remaining = run.bytes;
            while remaining > 0 {
                if segment >= bufs.len() {
                    return Ok(None);
                }
                let segment_len = bufs[segment].len();
                segment += 1;
                if segment_len == 0 {
                    continue;
                }
                if segment_len > remaining {
                    return Ok(None);
                }
                remaining -= segment_len;
            }
            let block_id = self.bdev.direct_physical_block_id(run.pblock);
            let Some(run_submission) = self
                .bdev
                .dev_mut()
                .try_read_blocks_vectored_async_submit(block_id, &mut bufs[start..segment])?
            else {
                return Ok(None);
            };
            submission.submit_batches += run_submission.submit_batches;
            submission.handles.extend(run_submission.handles);
        }

        record_async_mapped_read(runs.len(), bytes, submission.submit_batches);
        Ok(Some(submission))
    }

    pub(crate) fn alloc_inode(&mut self, ty: InodeType) -> Ext4Result<InodeRef<Hal>> {
        unsafe {
            let ty = match ty {
                InodeType::Fifo => EXT4_DE_FIFO,
                InodeType::CharacterDevice => EXT4_DE_CHRDEV,
                InodeType::Directory => EXT4_DE_DIR,
                InodeType::BlockDevice => EXT4_DE_BLKDEV,
                InodeType::RegularFile => EXT4_DE_REG_FILE,
                InodeType::Symlink => EXT4_DE_SYMLINK,
                InodeType::Socket => EXT4_DE_SOCK,
                InodeType::Unknown => EXT4_DE_UNKNOWN,
            };
            let mut result = InodeRef::new(mem::zeroed());
            ext4_fs_alloc_inode(self.inner.as_mut(), result.inner.as_mut(), ty as _)
                .context("ext4_fs_get_inode_ref")?;
            ext4_fs_inode_blocks_init(self.inner.as_mut(), result.inner.as_mut());
            Ok(result)
        }
    }

    pub fn get_attr(&mut self, ino: u32, attr: &mut FileAttr) -> Ext4Result<()> {
        self.inode_ref(ino)?.get_attr(attr);
        Ok(())
    }

    pub fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> Ext4Result<usize> {
        self.with_cached_inode_ref(ino, |inode| inode.read_at(buf, offset))
    }
    pub fn read_at_aligned_hot(
        &mut self,
        ino: u32,
        buf: &mut [u8],
        offset: u64,
    ) -> Ext4Result<usize> {
        if let Some(read) = self.try_read_at_aligned_hot_async(ino, buf, offset)? {
            return Ok(read);
        }
        self.with_cached_inode_ref(ino, |inode| inode.read_at_aligned_hot(buf, offset))
    }
    pub fn read_at_aligned_hot_vectored(
        &mut self,
        ino: u32,
        bufs: &mut [&mut [u8]],
        offset: u64,
    ) -> Ext4Result<Option<usize>> {
        let len = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        if len == 0 {
            return Ok(Some(0));
        }
        let Some((to_be_read, block_size, runs)) =
            self.mapped_aligned_read_plan(ino, offset, len)?
        else {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        };
        if to_be_read == 0 {
            return Ok(Some(0));
        }
        if !runs_align_segments(&runs, bufs.iter().map(|buf| buf.len())) {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        }
        if !segments_are_device_block_sized(bufs.iter().map(|buf| buf.len())) {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        }

        if let Some(read) =
            self.try_read_mapped_runs_async(ino, &runs, block_size, bufs, to_be_read)?
        {
            record_mapped_read_vectored(runs.len(), read);
            return Ok(Some(read));
        } else if async_mapped_read_enabled() {
            record_async_mapped_read_fallback();
        }

        let mut segment = 0usize;
        for run in &runs {
            let start = segment;
            let mut remaining = run.bytes;
            while remaining > 0 {
                let segment_len = bufs[segment].len();
                segment += 1;
                if segment_len == 0 {
                    continue;
                }
                debug_assert!(segment_len <= remaining);
                remaining -= segment_len;
            }
            let block_id = self.bdev.direct_physical_block_id(run.pblock);
            self.bdev
                .dev_mut()
                .read_blocks_vectored(block_id, &mut bufs[start..segment])?;
        }
        record_mapped_read_vectored(runs.len(), to_be_read);
        Ok(Some(to_be_read))
    }

    pub fn read_at_aligned_hot_vectored_async_submit(
        &mut self,
        ino: u32,
        bufs: &mut [&mut [u8]],
        offset: u64,
    ) -> Ext4Result<Option<AsyncReadSubmission>> {
        let len = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        if len == 0 {
            return Ok(Some(AsyncReadSubmission::default()));
        }
        let Some((to_be_read, block_size, runs)) =
            self.mapped_aligned_read_plan(ino, offset, len)?
        else {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        };
        if to_be_read == 0 {
            return Ok(Some(AsyncReadSubmission::default()));
        }
        if !runs_align_segments(&runs, bufs.iter().map(|buf| buf.len())) {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        }
        if !segments_are_device_block_sized(bufs.iter().map(|buf| buf.len())) {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        }

        let Some(submission) =
            self.try_read_mapped_runs_async_submit(ino, &runs, block_size, bufs, to_be_read)?
        else {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        };
        record_mapped_read_vectored(runs.len(), to_be_read);
        Ok(Some(submission))
    }
    pub fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> Ext4Result<usize> {
        self.with_cached_inode_ref(ino, |inode| inode.write_at(buf, offset))
    }
    pub fn write_at_aligned_hot(&mut self, ino: u32, buf: &[u8], offset: u64) -> Ext4Result<usize> {
        self.with_cached_inode_ref(ino, |inode| inode.write_at_aligned_hot(buf, offset))
    }
    pub fn write_at_aligned_hot_vectored(
        &mut self,
        ino: u32,
        bufs: &[&[u8]],
        offset: u64,
    ) -> Ext4Result<Option<usize>> {
        let len = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        if len == 0 {
            return Ok(Some(0));
        }
        let Some((block_size, runs)) = self.with_cached_inode_ref(ino, |inode| {
            let file_size = inode.size();
            let block_size = get_block_size(inode.superblock());
            let Some(end) = offset.checked_add(len as u64) else {
                return Ok(None);
            };
            if block_size != 4096
                || offset % block_size as u64 != 0
                || len % block_size as usize != 0
                || inode.inode_type() == InodeType::Symlink
                || end > file_size
            {
                return Ok(None);
            }
            let Some(runs) = inode.map_iomap_runs(offset, len, true)? else {
                return Ok(None);
            };
            if runs
                .iter()
                .any(|run| run.kind != MappedRunKind::Written || run.pblock == 0)
            {
                return Ok(None);
            }
            Ok(Some((block_size, runs)))
        })?
        else {
            return Ok(None);
        };
        if !runs_align_segments(&runs, bufs.iter().map(|buf| buf.len())) {
            return Ok(None);
        }
        if !segments_are_device_block_sized(bufs.iter().map(|buf| buf.len())) {
            return Ok(None);
        }

        let mut segment = 0usize;
        for run in &runs {
            let start = segment;
            let mut remaining = run.bytes;
            while remaining > 0 {
                let segment_len = bufs[segment].len();
                segment += 1;
                if segment_len == 0 {
                    continue;
                }
                debug_assert!(segment_len <= remaining);
                remaining -= segment_len;
            }
            let block_id = self.bdev.direct_physical_block_id(run.pblock);
            self.bdev
                .dev_mut()
                .write_blocks_vectored(block_id, &bufs[start..segment])?;
            self.bdev.invalidate_logical_block_range(
                run.pblock,
                (run.bytes / block_size as usize) as u32,
            );
        }
        record_mapped_overwrite_vectored_hit(len);
        Ok(Some(len))
    }

    pub fn write_at_aligned_hot_vectored_async_submit(
        &mut self,
        ino: u32,
        bufs: &[&[u8]],
        offset: u64,
    ) -> Ext4Result<Option<AsyncWriteSubmission>> {
        let len = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        if len == 0 {
            return Ok(Some(AsyncWriteSubmission::default()));
        }
        let Some((block_size, runs)) = self.with_cached_inode_ref(ino, |inode| {
            let file_size = inode.size();
            let block_size = get_block_size(inode.superblock());
            let Some(end) = offset.checked_add(len as u64) else {
                return Ok(None);
            };
            if block_size != 4096
                || offset % block_size as u64 != 0
                || len % block_size as usize != 0
                || inode.inode_type() == InodeType::Symlink
                || end > file_size
            {
                return Ok(None);
            }
            let Some(runs) = inode.map_iomap_runs(offset, len, true)? else {
                return Ok(None);
            };
            if runs
                .iter()
                .any(|run| run.kind != MappedRunKind::Written || run.pblock == 0)
            {
                return Ok(None);
            }
            Ok(Some((block_size, runs)))
        })?
        else {
            return Ok(None);
        };
        if !runs_align_segments(&runs, bufs.iter().map(|buf| buf.len())) {
            return Ok(None);
        }
        if !segments_are_device_block_sized(bufs.iter().map(|buf| buf.len())) {
            return Ok(None);
        }

        let mut segment = 0usize;
        let mut submission = AsyncWriteSubmission {
            bytes: len,
            ..AsyncWriteSubmission::default()
        };
        for run in &runs {
            let start = segment;
            let mut remaining = run.bytes;
            while remaining > 0 {
                let segment_len = bufs[segment].len();
                segment += 1;
                if segment_len == 0 {
                    continue;
                }
                debug_assert!(segment_len <= remaining);
                remaining -= segment_len;
            }
            let block_id = self.bdev.direct_physical_block_id(run.pblock);
            let Some(run_submission) = self
                .bdev
                .dev_mut()
                .try_write_blocks_vectored_async_submit(block_id, &bufs[start..segment])?
            else {
                return Ok(None);
            };
            submission.submit_batches += run_submission.submit_batches;
            submission.handles.extend(run_submission.handles);
            self.bdev.invalidate_logical_block_range(
                run.pblock,
                (run.bytes / block_size as usize) as u32,
            );
        }
        record_mapped_overwrite_vectored_hit(len);
        Ok(Some(submission))
    }
    pub fn is_block_aligned_range(&self, offset: u64, len: usize) -> bool {
        let block_size = get_block_size(&self.inner.as_ref().sb) as u64;
        block_size == 4096 && offset % block_size == 0 && len as u64 % block_size == 0
    }
    pub fn set_len(&mut self, ino: u32, len: u64) -> Ext4Result<()> {
        self.invalidate_hot_inode(ino);
        self.inode_ref(ino)?.set_len(len)?;
        self.invalidate_hot_inode(ino);
        Ok(())
    }
    pub fn set_symlink(&mut self, ino: u32, buf: &[u8]) -> Ext4Result<()> {
        self.invalidate_hot_inode(ino);
        self.inode_ref(ino)?.set_symlink(buf)?;
        self.invalidate_hot_inode(ino);
        Ok(())
    }
    pub fn lookup(&mut self, parent: u32, name: &str) -> Ext4Result<DirLookupResult<Hal>> {
        self.inode_ref(parent)?.lookup(name)
    }
    pub fn read_dir(&mut self, parent: u32, offset: u64) -> Ext4Result<DirReader<Hal>> {
        self.inode_ref(parent)?.read_dir(offset)
    }

    pub fn create(&mut self, parent: u32, name: &str, ty: InodeType, mode: u32) -> Ext4Result<u32> {
        self.drain_hot_inodes();
        let mut child = self.alloc_inode(ty)?;
        let mut parent = self.inode_ref(parent)?;
        parent.add_entry(name, &mut child)?;
        if ty == InodeType::Directory {
            child.add_entry(".", &mut self.clone_ref(&child))?;
            child.add_entry("..", &mut parent)?;
            assert_eq!(child.nlink(), 2);
        }
        child.set_mode((child.mode() & !0o777) | (mode & 0o777));

        Ok(child.ino())
    }

    pub fn rename(
        &mut self,
        src_dir: u32,
        src_name: &str,
        dst_dir: u32,
        dst_name: &str,
    ) -> Ext4Result {
        self.drain_hot_inodes();
        let mut src_dir_ref = self.inode_ref(src_dir)?;
        let mut dst_dir_ref = self.inode_ref(dst_dir)?;

        // TODO: optimize
        match self.unlink(dst_dir, dst_name) {
            Ok(_) => {}
            Err(err) if err.code == ENOENT as i32 => {}
            Err(err) => return Err(err),
        }

        let src = self.lookup(src_dir, src_name)?.entry().ino();

        let mut src_ref = self.inode_ref(src)?;
        if src_ref.is_dir() {
            let mut result = self.clone_ref(&src_ref).lookup("..")?;
            result.entry().raw_entry_mut().set_ino(dst_dir);
            src_dir_ref.dec_nlink();
            dst_dir_ref.inc_nlink();
        }
        dst_dir_ref.add_entry(dst_name, &mut src_ref)?;
        src_dir_ref.remove_entry(src_name, &mut src_ref)?;

        Ok(())
    }

    pub fn link(&mut self, dir: u32, name: &str, child: u32) -> Ext4Result {
        self.drain_hot_inodes();
        let mut child_ref = self.inode_ref(child)?;
        if child_ref.is_dir() {
            return Err(Ext4Error::new(EISDIR as _, "cannot link to directory"));
        }
        self.inode_ref(dir)?.add_entry(name, &mut child_ref)?;
        Ok(())
    }

    pub fn unlink(&mut self, dir: u32, name: &str) -> Ext4Result {
        self.drain_hot_inodes();
        let mut dir_ref = self.inode_ref(dir)?;
        let child = self.clone_ref(&dir_ref).lookup(name)?.entry().ino();
        let mut child_ref = self.inode_ref(child)?;
        let inode_type = child_ref.inode_type();

        if self.clone_ref(&child_ref).has_children()? {
            return Err(Ext4Error::new(ENOTEMPTY as _, None));
        }
        if inode_type == InodeType::Directory {
            // According to `ext4_trunc_dir`
            let bs = get_block_size(&self.inner.as_mut().sb);
            child_ref.truncate(bs as _)?;
        }

        dir_ref.remove_entry(name, &mut child_ref)?;

        if child_ref.is_dir() {
            dir_ref.dec_nlink();
            child_ref.dec_nlink();
        }
        if child_ref.nlink() == 0 {
            // Special inodes such as sockets and device nodes have no data
            // payload, and lwext4 rejects truncating them with EINVAL.
            if inode_needs_truncate_on_unlink(inode_type) {
                child_ref.truncate(0)?;
            }
            unsafe {
                ext4_inode_set_del_time(child_ref.inner.inode, u32::MAX);
                child_ref.mark_dirty();
                ext4_fs_free_inode(child_ref.inner.as_mut());
            }
        }
        Ok(())
    }

    pub fn stat(&mut self) -> Ext4Result<StatFs> {
        let sb = &mut self.inner.as_mut().sb;
        Ok(StatFs {
            inodes_count: u32::from_le(sb.inodes_count),
            free_inodes_count: u32::from_le(sb.free_inodes_count),
            blocks_count: (u32::from_le(sb.blocks_count_hi) as u64) << 32
                | u32::from_le(sb.blocks_count_lo) as u64,
            free_blocks_count: (u32::from_le(sb.free_blocks_count_hi) as u64) << 32
                | u32::from_le(sb.free_blocks_count_lo) as u64,
            block_size: get_block_size(sb),
        })
    }

    pub fn flush(&mut self) -> Ext4Result<()> {
        self.drain_hot_inodes();
        unsafe {
            ext4_block_cache_flush(self.bdev.inner.as_mut()).context("ext4_cache_flush")?;
        }
        self.bdev.dev_mut().flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_inodes_skip_truncate_during_unlink() {
        assert!(inode_needs_truncate_on_unlink(InodeType::RegularFile));
        assert!(inode_needs_truncate_on_unlink(InodeType::Directory));
        assert!(inode_needs_truncate_on_unlink(InodeType::Symlink));
        assert!(!inode_needs_truncate_on_unlink(InodeType::Socket));
        assert!(!inode_needs_truncate_on_unlink(InodeType::Fifo));
        assert!(!inode_needs_truncate_on_unlink(InodeType::CharacterDevice));
        assert!(!inode_needs_truncate_on_unlink(InodeType::BlockDevice));
    }
}

impl<Hal: SystemHal, Dev: BlockDevice> Drop for Ext4Filesystem<Hal, Dev> {
    fn drop(&mut self) {
        self.drain_hot_inodes();
        unsafe {
            let r = ext4_fs_fini(self.inner.as_mut());
            if r != 0 {
                log::error!("ext4_fs_fini failed: {}", Ext4Error::new(r, None));
            }
            let bdev = self.bdev.inner.as_mut();
            ext4_bcache_cleanup(bdev.bc);
            ext4_block_fini(bdev);
            ext4_bcache_fini_dynamic(bdev.bc);
        }
    }
}

pub(crate) struct WritebackGuard {
    bdev: *mut ext4_blockdev,
}
impl WritebackGuard {
    pub fn new(bdev: *mut ext4_blockdev) -> Self {
        unsafe { ext4_block_cache_write_back(bdev, 1) };
        Self { bdev }
    }
}
impl Drop for WritebackGuard {
    fn drop(&mut self) {
        unsafe { ext4_block_cache_write_back(self.bdev, 0) };
    }
}
