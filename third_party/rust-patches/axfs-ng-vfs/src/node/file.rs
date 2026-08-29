use alloc::{sync::Arc, vec::Vec};
use core::ops::Deref;

use axpoll::Pollable;

use super::NodeOps;
use crate::{VfsError, VfsResult};

/// Maximum number of file extents a normal FIEMAP caller retains in one
/// query.  The typed VFS API still accepts a caller supplied `max_extents`;
/// this constant is available to adapters which want to impose the Linux
/// ioctl's bounded capacity before entering a filesystem implementation.
pub const FILE_EXTENT_MAX: usize = 4096;

/// Maximum logical range mapped while a filesystem-specific spin lock is
/// held.  Callers release and reacquire the backend lock between chunks.
pub const FILE_EXTENT_SCAN_CHUNK_BYTES: u64 = 16 * 1024 * 1024;

/// One allocated file extent returned by a filesystem mapping query.  This
/// includes unwritten allocations; the kernel adapter preserves their typed
/// FIEMAP flag and performs usercopy only after the filesystem lock has been
/// released.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileExtent {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
    /// Filesystem-independent FIEMAP flags.  The VFS does not define Linux
    /// userspace structs, but bit 0 is reserved for `FIEMAP_EXTENT_LAST` by
    /// the typed lower contract.
    pub flags: u32,
}

impl FileExtent {
    pub const fn new(logical: u64, physical: u64, length: u64, flags: u32) -> Self {
        Self {
            logical,
            physical,
            length,
            flags,
        }
    }
}

/// Typed result of one file extent mapping query.
#[derive(Debug, Eq, PartialEq)]
pub struct FileExtentMap {
    /// The retained prefix.  It is empty for a count-only query.
    pub extents: Vec<FileExtent>,
    /// For a non-zero capacity this is the number of retained extents, not
    /// the total discovered count.  A zero-capacity query returns the exact
    /// discovered count here without allocating `extents`.
    pub mapped_extents: u32,
    /// Whether the complete range was scanned and all mapped extents were
    /// retained.  Count-only scans are complete by definition.
    pub complete: bool,
}

impl FileExtentMap {
    pub fn new(extents: Vec<FileExtent>, mapped_extents: u32, complete: bool) -> Self {
        Self {
            extents,
            mapped_extents,
            complete,
        }
    }
}

/// One caller-owned physical-memory range used by a synchronous direct I/O
/// request.
///
/// The range must remain pinned, DMA-accessible, and disjoint from every other
/// range for the complete call. This descriptor deliberately carries no
/// allocator, address-space, or driver dependency; the owner of the range is
/// responsible for its lifetime, access permissions, and any content race
/// between CPU access and device DMA.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalIoSegment {
    /// Physical address of the first byte in the range.
    pub paddr: usize,
    /// Number of bytes in the range.
    pub len: usize,
}

impl PhysicalIoSegment {
    pub const fn new(paddr: usize, len: usize) -> Self {
        Self { paddr, len }
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Result of attempting a synchronous physical direct-I/O request.
///
/// `NotSubmitted` is deliberately typed: callers may use their pre-publish
/// fallback only for an operation which never reached the device.  Once a
/// lower layer returns `Completed`, later validation errors remain terminal
/// and must not be retried through a bounce buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoAttempt {
    Completed(usize),
    NotSubmitted(PhysicalIoNotSubmittedReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoNotSubmittedReason {
    /// The filesystem mapping or direct-I/O preflight did not admit the
    /// request (for example a hole, fragmented extent, or EOF range).
    Extent,
    /// The request was eligible in the filesystem, but device admission did
    /// not publish a descriptor (for example queue capacity or unsupported
    /// physical SG geometry).
    DeviceAdmission,
}

/// Result of an asynchronous vectored write attempt.
///
/// Submission/admission failures remain the outer [`VfsResult`] error: no
/// device request was accepted, so callers may retain their dirty state and
/// must not report a writeback completion error.  Once a request is accepted,
/// implementations return one of the two terminal outcomes below only after
/// it has completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AsyncVectoredWriteOutcome {
    /// No asynchronous request was accepted and the caller may use its
    /// synchronous fallback path.
    NotSubmitted,
    /// An accepted request completed successfully.
    Completed(usize),
    /// An accepted request completed with this error.
    CompletionError(VfsError),
}

pub trait FileNodeOps: NodeOps + Pollable {
    /// Collects allocated file extents intersecting `[start, start + length)`.
    /// Holes are omitted.  A zero capacity is a complete count-only scan and
    /// must not allocate an extent buffer; a non-zero capacity retains only a
    /// prefix and reports the retained count in `mapped_extents`.
    ///
    /// No userspace pointers or Linux ABI structs cross this VFS boundary.
    fn map_extents(
        &self,
        start: u64,
        length: u64,
        max_extents: usize,
    ) -> VfsResult<FileExtentMap> {
        let _ = (start, length, max_extents);
        Err(VfsError::OperationNotSupported)
    }

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

    /// Attempts a synchronous direct read into caller-pinned physical memory.
    ///
    /// The implementation must not construct a Rust slice from a physical
    /// address. `Ok(None)` means that the caller may use its ordinary fallback
    /// path; an error is terminal for this request and must not be treated as
    /// permission to retry through a bounce buffer.
    ///
    /// # Safety
    ///
    /// Every non-empty segment must remain pinned, DMA-accessible, writable,
    /// and disjoint from all other segments until this method returns. The
    /// caller is responsible for content races caused by concurrent CPU access
    /// to the DMA range; such races do not create Rust references from paddr.
    unsafe fn try_read_at_physical(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        let _ = (segments, offset);
        Ok(None)
    }

    /// Typed form of [`Self::try_read_at_physical`].  The default adapter is
    /// intentionally conservative: an implementation which has no physical
    /// hook reports device admission failure, which is still unpublished and
    /// therefore safe for the caller's fallback path.
    unsafe fn try_read_at_physical_with_reason(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        Ok(match unsafe { self.try_read_at_physical(segments, offset)? } {
            Some(bytes) => PhysicalIoAttempt::Completed(bytes),
            None => PhysicalIoAttempt::NotSubmitted(
                PhysicalIoNotSubmittedReason::DeviceAdmission,
            ),
        })
    }

    /// Performs a side-effect-free capability and mapping preflight for a
    /// physical read.  The high-level direct backend runs this while holding
    /// its direct-I/O exclusion, before it invalidates cached pages; a true
    /// result therefore remains eligible until the matching unsafe hook call.
    fn physical_read_eligible(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<bool> {
        let _ = (segments, offset);
        Ok(false)
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
    /// filesystem locks across a blocking wait. [`AsyncVectoredWriteOutcome::NotSubmitted`]
    /// means the caller should use [`write_at_vectored`](Self::write_at_vectored).
    /// Errors in the outer result occurred before a request was accepted.
    fn try_write_at_vectored_async(
        &self,
        bufs: &[&[u8]],
        offset: u64,
    ) -> VfsResult<AsyncVectoredWriteOutcome> {
        let _ = bufs;
        let _ = offset;
        Ok(AsyncVectoredWriteOutcome::NotSubmitted)
    }

    /// Attempts a synchronous direct overwrite from caller-pinned physical
    /// memory. The operation must not extend the file.
    ///
    /// # Safety
    ///
    /// Every non-empty segment must remain pinned, DMA-accessible, readable,
    /// and disjoint from all other segments until this method returns. The
    /// caller is responsible for content races caused by concurrent CPU access
    /// to the DMA range; such races do not create Rust references from paddr.
    unsafe fn try_write_at_physical(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<Option<usize>> {
        let _ = (segments, offset);
        Ok(None)
    }

    /// Typed form of [`Self::try_write_at_physical`]; see the read-side
    /// contract for the publish boundary and fallback rule.
    unsafe fn try_write_at_physical_with_reason(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<PhysicalIoAttempt> {
        Ok(match unsafe { self.try_write_at_physical(segments, offset)? } {
            Some(bytes) => PhysicalIoAttempt::Completed(bytes),
            None => PhysicalIoAttempt::NotSubmitted(
                PhysicalIoNotSubmittedReason::DeviceAdmission,
            ),
        })
    }

    /// Performs a side-effect-free capability and mapping preflight for a
    /// physical overwrite.  It must not publish a descriptor or touch file
    /// data; the high-level direct backend calls the unsafe hook only after a
    /// true result and cache invalidation under the same direct-I/O lock.
    fn physical_write_eligible(
        &self,
        segments: &[PhysicalIoSegment],
        offset: u64,
    ) -> VfsResult<bool> {
        let _ = (segments, offset);
        Ok(false)
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

    /// Clones the node's owned trait object and downcasts it without requiring
    /// an `Arc<FileNode>` at the call site.  The returned `Arc` is an owned
    /// worker-safe inode reference; no VFS borrow crosses an await boundary.
    pub fn downcast_owned<T: FileNodeOps>(&self) -> VfsResult<Arc<T>> {
        self.0
            .clone()
            .into_any()
            .downcast()
            .map_err(|_| VfsError::InvalidInput)
    }
}
