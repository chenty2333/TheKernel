use alloc::sync::Arc;
use core::ops::Deref;

use axpoll::Pollable;

use super::NodeOps;
use crate::{VfsError, VfsResult};

pub trait FileNodeOps: NodeOps + Pollable {
    /// Reads a number of bytes starting from a given offset.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize>;

    /// Reads data into a scatter list starting from a given offset.
    fn read_at_vectored(&self, bufs: &mut [&mut [u8]], mut offset: u64) -> VfsResult<usize> {
        let mut total = 0usize;
        for buf in bufs.iter_mut() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let read = match self.read_at(buf, offset) {
                Ok(read) => read,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += read;
            offset = offset
                .checked_add(read as u64)
                .ok_or(VfsError::InvalidInput)?;
            if read < requested || read == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Attempts to read data into a scatter list through an asynchronous
    /// lower-device path.
    ///
    /// Implementations must return only after accepted device requests have
    /// completed, but may split submit and wait internally to avoid holding
    /// filesystem locks across a blocking wait. `Ok(None)` means the caller
    /// should use [`read_at_vectored`](Self::read_at_vectored).
    fn try_read_at_vectored_async(
        &self,
        bufs: &mut [&mut [u8]],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        let _ = bufs;
        let _ = offset;
        Ok(None)
    }

    /// Writes a number of bytes starting from a given offset.
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize>;

    /// Writes data from a scatter list starting from a given offset.
    fn write_at_vectored(&self, bufs: &[&[u8]], mut offset: u64) -> VfsResult<usize> {
        let mut total = 0usize;
        for buf in bufs.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            let requested = buf.len();
            let written = match self.write_at(buf, offset) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += written;
            offset = offset
                .checked_add(written as u64)
                .ok_or(VfsError::InvalidInput)?;
            if written < requested || written == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Attempts to write data from a scatter list through an asynchronous
    /// lower-device path.
    ///
    /// Implementations must return only after accepted device requests have
    /// completed, but may split submit and wait internally to avoid holding
    /// filesystem locks across a blocking wait. `Ok(None)` means the caller
    /// should use [`write_at_vectored`](Self::write_at_vectored).
    fn try_write_at_vectored_async(&self, bufs: &[&[u8]], offset: u64) -> VfsResult<Option<usize>> {
        let _ = bufs;
        let _ = offset;
        Ok(None)
    }

    /// Appends data to the file.
    ///
    /// Returns `(written, offset)` where `written` is the number of bytes
    /// written and `offset` is the new file size.
    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)>;

    /// Sets the size of the file.
    ///
    /// Unless [`set_len_failure_is_atomic`](Self::set_len_failure_is_atomic)
    /// returns `true`, an error may be reported after the implementation has
    /// changed file data, allocation metadata, or the visible length. Cache
    /// users must therefore invalidate any pages that could have become stale.
    fn set_len(&self, len: u64) -> VfsResult<()>;

    /// Whether a failed [`set_len`](Self::set_len) leaves all file data,
    /// allocation metadata, and the visible length unchanged.
    ///
    /// Implementations must return `true` only when this is a stable guarantee
    /// for every error path. Callers may retain and restore pre-operation cache
    /// pages based on this contract. The conservative default is `false`.
    fn set_len_failure_is_atomic(&self) -> bool {
        false
    }

    /// Sets the file's symlink target.
    fn set_symlink(&self, target: &str) -> VfsResult<()>;

    /// Manipulates the underlying device parameters of special files.
    fn ioctl(&self, _cmd: u32, _arg: usize) -> VfsResult<usize> {
        Err(VfsError::NotATty)
    }
}

#[repr(transparent)]
pub struct FileNode(Arc<dyn FileNodeOps>);

impl Deref for FileNode {
    type Target = dyn FileNodeOps;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl From<FileNode> for Arc<dyn NodeOps> {
    fn from(node: FileNode) -> Self {
        node.0.clone()
    }
}

impl FileNode {
    pub fn new(ops: Arc<dyn FileNodeOps>) -> Self {
        Self(ops)
    }

    pub fn inner(&self) -> &Arc<dyn FileNodeOps> {
        &self.0
    }

    pub fn downcast<T: FileNodeOps>(self: &Arc<Self>) -> VfsResult<Arc<T>> {
        self.0
            .clone()
            .into_any()
            .downcast()
            .map_err(|_| VfsError::InvalidInput)
    }
}
