mod attr;
mod dir;
mod file;
mod xattr;

use alloc::boxed::Box;
use core::{marker::PhantomData, mem};

pub use attr::{FileAttr, Timestamp};
pub use dir::{DirEntry, DirLookupResult, DirReader};

use crate::{Ext4Error, Ext4Result, SystemHal, error::Context, ffi::*, hot::ExtentStatusCache};

/// Persistent identity of an inode slot.
///
/// An inode number alone is not sufficient once the slot has been freed and
/// reused.  The on-disk generation is advanced for every allocation, so a
/// retained handle can verify that deferred release or deletion still refers
/// to the same inode instance.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InodeToken {
    ino: u32,
    generation: u32,
}

impl InodeToken {
    pub(crate) const fn new(ino: u32, generation: u32) -> Self {
        Self { ino, generation }
    }

    pub const fn ino(self) -> u32 {
        self.ino
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }
}

/// Inode type.
#[repr(u8)]
#[derive(PartialEq, Default, Eq, Clone, Copy, Debug)]
pub enum InodeType {
    #[default]
    Unknown         = 0,
    Fifo            = 1,
    CharacterDevice = 2,
    Directory       = 4,
    BlockDevice     = 6,
    RegularFile     = 8,
    Symlink         = 10,
    Socket          = 12,
}
impl From<u8> for InodeType {
    fn from(value: u8) -> Self {
        match value {
            1 => InodeType::Fifo,
            2 => InodeType::CharacterDevice,
            4 => InodeType::Directory,
            6 => InodeType::BlockDevice,
            8 => InodeType::RegularFile,
            10 => InodeType::Symlink,
            12 => InodeType::Socket,
            _ => InodeType::Unknown,
        }
    }
}

pub struct InodeRef<Hal: SystemHal> {
    pub(crate) inner: Box<ext4_inode_ref>,
    pub(crate) extent_status: ExtentStatusCache,
    pub(crate) mapping_seq: u64,
    released: bool,
    _phantom: PhantomData<Hal>,
}
impl<Hal: SystemHal> InodeRef<Hal> {
    /// Admits every allocation used by an inode reference before the C side
    /// acquires or allocates the corresponding on-disk inode. The initially
    /// released state makes dropping a never-activated preparation a no-op.
    pub(crate) fn try_uninitialized() -> Ext4Result<Self> {
        let inner = Box::try_new(unsafe { mem::zeroed() })
            .map_err(|_| Ext4Error::new(ENOMEM as _, "ext4 inode-reference allocation failed"))?;
        Ok(Self {
            inner,
            extent_status: ExtentStatusCache::try_new()?,
            mapping_seq: 0,
            released: true,
            _phantom: PhantomData,
        })
    }

    pub(crate) fn activate(&mut self) {
        self.released = false;
    }

    pub fn ino(&self) -> u32 {
        self.inner.index
    }

    pub fn generation(&self) -> u32 {
        unsafe { ext4_inode_get_generation(self.inner.inode) }
    }

    pub fn token(&self) -> InodeToken {
        InodeToken::new(self.ino(), self.generation())
    }

    fn put(&mut self) -> Ext4Result<()> {
        if self.released {
            return Ok(());
        }
        // A failed put must not be retried from Drop: the C side may already
        // have consumed the block-cache reference before reporting an error.
        self.released = true;
        let metadata_may_have_changed = self.inner.dirty;
        unsafe { ext4_fs_put_inode_ref(self.inner.as_mut()) }
            .context("ext4_fs_put_inode_ref")
            .map_err(|error| error.with_metadata_may_have_changed(metadata_may_have_changed))
    }

    /// Explicitly release the inode reference and propagate writeback errors.
    pub fn finish(mut self) -> Ext4Result<()> {
        self.put()
    }

    pub(crate) fn superblock(&self) -> &ext4_sblock {
        unsafe { &(*self.inner.fs).sb }
    }
    pub(crate) fn superblock_mut(&mut self) -> &mut ext4_sblock {
        unsafe { &mut (*self.inner.fs).sb }
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.inner.dirty = true;
    }

    pub(crate) fn invalidate_mapping_seq(&mut self) {
        self.mapping_seq = self.mapping_seq.wrapping_add(1);
    }

    pub(crate) fn inc_nlink(&mut self) {
        unsafe {
            ext4_fs_inode_links_count_inc(self.inner.as_mut());
        }
        self.mark_dirty();
    }

    pub(crate) fn ensure_can_inc_nlink(&self) -> Ext4Result<()> {
        let nlink = self.nlink();
        let indexed_directory = self.is_dir()
            && u32::from_le(self.superblock().features_compatible) & EXT4_FCOM_DIR_INDEX != 0
            && unsafe { ext4_inode_has_flag(self.inner.inode, EXT4_INODE_FLAG_INDEX) };

        // Indexed directories use nlink == 1 as the DIR_NLINK overflow
        // sentinel. Other inode types must fail before the directory entry is
        // published. Even an indexed directory must not wrap u16::MAX to zero
        // if an already-corrupt image reaches this path.
        if (!indexed_directory && nlink >= EXT4_LINK_MAX as u16)
            || (indexed_directory && nlink == u16::MAX)
        {
            return Err(Ext4Error::new(EMLINK as _, "ext4 link count limit reached"));
        }
        Ok(())
    }

    pub(crate) fn dec_parent_dir_nlink(&mut self) {
        debug_assert!(self.is_dir());
        unsafe {
            ext4_fs_inode_links_count_dec(self.inner.as_mut());
        }
        self.mark_dirty();
    }

    pub(crate) fn dec_nlink(&mut self) {
        self.set_nlink(self.nlink() - 1);
        self.mark_dirty();
    }

    pub(crate) fn set_nlink(&mut self, nlink: u16) {
        self.raw_inode_mut().links_count = u16::to_le(nlink);
        self.mark_dirty();
    }

    pub(crate) fn raw_inode(&self) -> &ext4_inode {
        unsafe { &*self.inner.inode }
    }
    pub(crate) fn raw_inode_mut(&mut self) -> &mut ext4_inode {
        unsafe { &mut *self.inner.inode }
    }
}

impl<Hal: SystemHal> Drop for InodeRef<Hal> {
    fn drop(&mut self) {
        if let Err(err) = self.put() {
            log::error!("failed to release ext4 inode reference: {err}");
        }
    }
}
