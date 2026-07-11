use core::{mem, slice};

use super::{InodeRef, InodeType};
use crate::{Ext4Error, Ext4Result, SystemHal, error::Context, ffi::*, util::revision_tuple};

fn preserve_primary_error(primary: Ext4Error, cleanup: Ext4Result<()>) -> Ext4Error {
    match cleanup {
        Ok(()) => primary,
        Err(err) => {
            log::error!("secondary ext4 directory cleanup failure: {err}");
            primary.with_metadata_may_have_changed(err.metadata_may_have_changed())
        }
    }
}

fn preserve_lookup_error(primary: Ext4Error, cleanup: Ext4Result<()>) -> Ext4Error {
    match cleanup {
        // ENOENT is control flow for create and rename. Never let it mask a
        // cleanup failure and authorize a following metadata mutation.
        Err(cleanup) if primary.code == ENOENT as i32 => {
            cleanup.with_metadata_may_have_changed(primary.metadata_may_have_changed())
        }
        cleanup => preserve_primary_error(primary, cleanup),
    }
}

fn combine_directory_cleanup(first: Ext4Result<()>, second: Ext4Result<()>) -> Ext4Result<()> {
    match (first, second) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(err), Err(secondary)) => {
            log::error!("secondary ext4 directory cleanup failure: {secondary}");
            Err(err.with_metadata_may_have_changed(secondary.metadata_may_have_changed()))
        }
    }
}

impl<Hal: SystemHal> InodeRef<Hal> {
    pub fn read_dir(mut self, offset: u64) -> Ext4Result<DirReader<Hal>> {
        unsafe {
            let mut iter = mem::zeroed();
            if let Err(err) = ext4_dir_iterator_init(&mut iter, self.inner.as_mut(), offset)
                .context("ext4_dir_iterator_init")
            {
                let iterator_cleanup =
                    ext4_dir_iterator_fini(&mut iter).context("ext4_dir_iterator_fini");
                let cleanup = combine_directory_cleanup(iterator_cleanup, self.finish());
                return Err(preserve_primary_error(err, cleanup));
            }

            Ok(DirReader {
                parent: Some(self),
                inner: iter,
            })
        }
    }

    pub fn lookup(mut self, name: &str) -> Ext4Result<DirLookupResult<Hal>> {
        unsafe {
            let mut result = mem::zeroed();
            let mut metadata_may_have_changed = false;
            if let Err(err) = ext4_dir_find_entry_status(
                &mut result,
                self.inner.as_mut(),
                name.as_ptr() as *const _,
                name.len() as _,
                &mut metadata_may_have_changed,
            )
            .context("ext4_dir_find_entry")
            .map_err(|error| error.with_metadata_may_have_changed(metadata_may_have_changed))
            {
                // Indexed lookup may have published a leaf block in `result`
                // before failing while releasing its index path. Always tear
                // down the zero-initialized/partially initialized result.
                let result_cleanup = ext4_dir_destroy_result(self.inner.as_mut(), &mut result)
                    .context("ext4_dir_destroy_result");
                let cleanup = combine_directory_cleanup(result_cleanup, self.finish());
                return Err(preserve_lookup_error(err, cleanup));
            }

            Ok(DirLookupResult {
                parent: Some(self),
                inner: result,
                entry_modified: false,
            })
        }
    }

    pub fn has_children(self) -> Ext4Result<bool> {
        if self.inode_type() != InodeType::Directory {
            self.finish()?;
            return Ok(false);
        }
        let mut reader = self.read_dir(0)?;
        while let Some(curr) = reader.current() {
            let name = curr.name();
            if name != b"." && name != b".." {
                reader.finish()?;
                return Ok(true);
            }
            if let Err(err) = reader.step() {
                return Err(preserve_primary_error(err, reader.finish()));
            }
        }
        reader.finish()?;
        Ok(false)
    }

    pub(crate) fn add_entry(&mut self, name: &str, entry: &mut InodeRef<Hal>) -> Ext4Result {
        entry.ensure_can_inc_nlink()?;
        unsafe {
            let mut metadata_may_have_changed = false;
            ext4_dir_add_entry_status(
                self.inner.as_mut(),
                name.as_ptr() as *const _,
                name.len() as _,
                entry.inner.as_mut(),
                &mut metadata_may_have_changed,
            )
            .context("ext4_dir_add_entry")
            .map_err(|error| error.with_metadata_may_have_changed(metadata_may_have_changed))?;
        }
        entry.inc_nlink();
        Ok(())
    }
    pub(crate) fn remove_entry(&mut self, name: &str, entry: &mut InodeRef<Hal>) -> Ext4Result {
        unsafe {
            let mut metadata_may_have_changed = false;
            ext4_dir_remove_entry_status(
                self.inner.as_mut(),
                name.as_ptr() as *const _,
                name.len() as _,
                &mut metadata_may_have_changed,
            )
            .context("ext4_dir_remove_entry")
            .map_err(|error| error.with_metadata_may_have_changed(metadata_may_have_changed))?;
        }
        entry.dec_nlink();
        Ok(())
    }
}

pub struct DirLookupResult<Hal: SystemHal> {
    parent: Option<InodeRef<Hal>>,
    inner: ext4_dir_search_result,
    entry_modified: bool,
}
impl<Hal: SystemHal> DirLookupResult<Hal> {
    pub fn entry(&mut self) -> Ext4Result<DirEntry<'_>> {
        let Some(parent) = self.parent.as_ref() else {
            return Err(Ext4Error::new(
                EIO as _,
                "ext4 directory lookup result was already released",
            ));
        };
        Ok(DirEntry {
            inner: unsafe { &mut *(self.inner.dentry as *mut _) },
            sb: parent.superblock(),
        })
    }

    pub(crate) fn set_entry_ino(&mut self, ino: u32) -> Ext4Result<()> {
        self.entry()?.raw_entry_mut().set_ino(ino);
        self.entry_modified = true;
        let Some(parent) = self.parent.as_mut() else {
            return Err(Ext4Error::new(
                EIO as _,
                "ext4 directory lookup result was already released",
            ));
        };
        unsafe {
            ext4_dir_set_csum(parent.inner.as_mut(), self.inner.block.data.cast());
            ext4_trans_set_block_dirty(self.inner.block.buf)
                .context("ext4_trans_set_block_dirty")
                .map_err(|error| error.with_metadata_may_have_changed(true))?;
        }
        Ok(())
    }

    fn release(&mut self) -> Ext4Result<()> {
        let Some(mut parent) = self.parent.take() else {
            return Ok(());
        };
        let result = unsafe { ext4_dir_destroy_result(parent.inner.as_mut(), &mut self.inner) }
            .context("ext4_dir_destroy_result")
            .map_err(|error| error.with_metadata_may_have_changed(self.entry_modified));
        combine_directory_cleanup(result, parent.finish())
    }

    pub fn finish(mut self) -> Ext4Result<()> {
        self.release()
    }
}
impl<Hal: SystemHal> Drop for DirLookupResult<Hal> {
    fn drop(&mut self) {
        if let Err(err) = self.release() {
            log::error!("failed to release ext4 directory lookup result: {err}");
        }
    }
}

#[repr(transparent)]
pub struct RawDirEntry {
    inner: ext4_dir_en,
}
impl RawDirEntry {
    pub fn ino(&self) -> u32 {
        u32::from_le(self.inner.inode)
    }
    pub fn set_ino(&mut self, ino: u32) {
        self.inner.inode = u32::to_le(ino);
    }

    pub fn len(&self) -> u16 {
        u16::from_le(self.inner.entry_len)
    }

    pub fn name<'a>(&'a self, sb: &ext4_sblock) -> &'a [u8] {
        let mut name_len = self.inner.name_len as u16;
        if revision_tuple(sb) < (0, 5) {
            let high = unsafe { self.inner.in_.name_length_high };
            name_len |= (high as u16) << 8;
        }
        unsafe { slice::from_raw_parts(self.inner.name.as_ptr(), name_len as usize) }
    }

    pub fn inode_type(&self, sb: &ext4_sblock) -> InodeType {
        if revision_tuple(sb) < (0, 5) {
            InodeType::Unknown
        } else {
            match unsafe { self.inner.in_.inode_type } as u32 {
                EXT4_DE_DIR => InodeType::Directory,
                EXT4_DE_REG_FILE => InodeType::RegularFile,
                EXT4_DE_SYMLINK => InodeType::Symlink,
                EXT4_DE_CHRDEV => InodeType::CharacterDevice,
                EXT4_DE_BLKDEV => InodeType::BlockDevice,
                EXT4_DE_FIFO => InodeType::Fifo,
                EXT4_DE_SOCK => InodeType::Socket,
                _ => InodeType::Unknown,
            }
        }
    }
}

pub struct DirEntry<'a> {
    inner: &'a mut RawDirEntry,
    sb: &'a ext4_sblock,
}
impl DirEntry<'_> {
    pub fn ino(&self) -> u32 {
        self.inner.ino()
    }

    pub fn name(&self) -> &[u8] {
        self.inner.name(self.sb)
    }

    pub fn inode_type(&self) -> InodeType {
        self.inner.inode_type(self.sb)
    }

    pub fn len(&self) -> u16 {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.len() == 0
    }

    pub fn raw_entry(&self) -> &RawDirEntry {
        self.inner
    }
    pub fn raw_entry_mut(&mut self) -> &mut RawDirEntry {
        self.inner
    }
}

/// Reader returned by [`InodeRef::read_dir`].
pub struct DirReader<Hal: SystemHal> {
    parent: Option<InodeRef<Hal>>,
    inner: ext4_dir_iter,
}
impl<Hal: SystemHal> DirReader<Hal> {
    pub fn current(&self) -> Option<DirEntry<'_>> {
        if self.inner.curr.is_null() {
            return None;
        }
        let curr = unsafe { &mut *(self.inner.curr as *mut _) };
        let sb = self.parent.as_ref()?.superblock();

        Some(DirEntry { inner: curr, sb })
    }

    pub fn step(&mut self) -> Ext4Result {
        if !self.inner.curr.is_null() {
            unsafe {
                ext4_dir_iterator_next(&mut self.inner).context("ext4_dir_iterator_next")?;
            }
        }
        Ok(())
    }

    pub fn offset(&self) -> u64 {
        self.inner.curr_off
    }

    fn release(&mut self) -> Ext4Result<()> {
        let Some(parent) = self.parent.take() else {
            return Ok(());
        };
        let result =
            unsafe { ext4_dir_iterator_fini(&mut self.inner) }.context("ext4_dir_iterator_fini");
        combine_directory_cleanup(result, parent.finish())
    }

    pub fn finish(mut self) -> Ext4Result<()> {
        self.release()
    }
}
impl<Hal: SystemHal> Drop for DirReader<Hal> {
    fn drop(&mut self) {
        if let Err(err) = self.release() {
            log::error!("failed to release ext4 directory iterator: {err}");
        }
    }
}
