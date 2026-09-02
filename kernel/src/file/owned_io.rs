//! Kernel ownership of user pages submitted to provider-owned file I/O.
//!
//! This is deliberately a physical-copy adapter, not a user-slice adapter.
//! A request pins and validates its user ranges at submission, retains those
//! pins until the provider returns its `FileIoRequest`, and later copies by
//! walking the captured physical SG descriptors.  In particular, a worker
//! never reparses a userspace address and never receives a long-lived Rust
//! slice into userspace.

use alloc::vec::Vec;

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{
    FileIoBufferAccess, FileIoPhysicalSegment, FileIoPhysicalSegmentVisitor, OwnedFileIoBuffer,
    VfsError, VfsResult,
};
use axio::{Read, Write};

use crate::mm::{
    IoVectorBuf, PinnedPhysicalReader, PinnedPhysicalWriter, PinnedUserSegments,
    PinnedUserSegmentsMut, UserIoPinSegment, UserMemoryCapability,
    pin_user_segments_from_user_longterm_with, pin_user_segments_to_user_longterm_with,
};

struct PinnedChunk<T> {
    /// This is the submitted (and hence visible) byte count, rather than an
    /// inferred page-rounded descriptor length.
    len: usize,
    pin: T,
}

enum PinnedFileIoDirection {
    Source(Vec<PinnedChunk<PinnedUserSegments>>),
    Destination(Vec<PinnedChunk<PinnedUserSegmentsMut>>),
}

/// An owned `axfs_ng_vfs::OwnedFileIoBuffer` backed by long-term MM pins.
///
/// It is the sole owner of the `PinnedUserSegments{,Mut}` objects.  Dropping
/// the provider request consequently releases the long-term MM reservation;
/// neither submission callers nor provider workers need a userspace virtual
/// address after construction.
pub(crate) struct OwnedPinnedFileIoBuffer {
    len: usize,
    direction: PinnedFileIoDirection,
}

// `PinnedUserSegments` retains a raw user address solely for accounting and
// eventual MM-pin release; this adapter never dereferences it.  All later I/O
// goes through the captured physical descriptors while their owners remain
// alive.  Destination copying requires `&mut self`, so its mutable pins cannot
// be written through concurrently via this buffer.
unsafe impl Send for OwnedPinnedFileIoBuffer {}
unsafe impl Sync for OwnedPinnedFileIoBuffer {}

impl OwnedPinnedFileIoBuffer {
    /// Pins one source range for a provider-owned write operation.
    pub(crate) fn pin_source(
        capability: &UserMemoryCapability,
        address: *const u8,
        len: usize,
    ) -> AxResult<Self> {
        if len == 0 {
            return Ok(Self::empty_source());
        }
        let mut pins = Vec::new();
        pins.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
        let pin = pin_user_segments_from_user_longterm_with(capability, address, len)?;
        pins.push(PinnedChunk { len, pin });
        Ok(Self {
            len,
            direction: PinnedFileIoDirection::Source(pins),
        })
    }

    /// Pins one destination range for a provider-owned read operation.
    pub(crate) fn pin_destination(
        capability: &UserMemoryCapability,
        address: *mut u8,
        len: usize,
    ) -> AxResult<Self> {
        if len == 0 {
            return Ok(Self::empty_destination());
        }
        let mut pins = Vec::new();
        pins.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
        let pin = pin_user_segments_to_user_longterm_with(capability, address, len)?;
        pins.push(PinnedChunk { len, pin });
        Ok(Self {
            len,
            direction: PinnedFileIoDirection::Destination(pins),
        })
    }

    /// Pins the leading `len` bytes of an already imported write iovec.
    /// Iovec descriptors are read during syscall submission by `IoVectorBuf`;
    /// this method never reloads the userspace iovec array.
    pub(crate) fn pin_iov_source(iov: &IoVectorBuf, len: usize) -> AxResult<Self> {
        if len > iov.len() {
            return Err(AxError::InvalidInput);
        }
        if len == 0 {
            return Ok(Self::empty_source());
        }
        let mut pins = Vec::new();
        pins.try_reserve_exact(iov.iovcnt())
            .map_err(|_| AxError::NoMemory)?;
        let mut remaining = len;
        for index in 0..iov.iovcnt() {
            if remaining == 0 {
                break;
            }
            let entry = iov.entry(index)?;
            let chunk = (entry.iov_len as usize).min(remaining);
            if chunk == 0 {
                continue;
            }
            let pin = pin_user_segments_from_user_longterm_with(
                iov.capability(),
                entry.iov_base as usize as *const u8,
                chunk,
            )?;
            pins.push(PinnedChunk { len: chunk, pin });
            remaining -= chunk;
        }
        if remaining != 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            len,
            direction: PinnedFileIoDirection::Source(pins),
        })
    }

    /// Pins the leading `len` bytes of an already imported read iovec.
    pub(crate) fn pin_iov_destination(iov: &IoVectorBuf, len: usize) -> AxResult<Self> {
        if len > iov.len() {
            return Err(AxError::InvalidInput);
        }
        if len == 0 {
            return Ok(Self::empty_destination());
        }
        let mut pins = Vec::new();
        pins.try_reserve_exact(iov.iovcnt())
            .map_err(|_| AxError::NoMemory)?;
        let mut remaining = len;
        for index in 0..iov.iovcnt() {
            if remaining == 0 {
                break;
            }
            let entry = iov.entry(index)?;
            let chunk = (entry.iov_len as usize).min(remaining);
            if chunk == 0 {
                continue;
            }
            let pin = pin_user_segments_to_user_longterm_with(
                iov.capability(),
                entry.iov_base as usize as *mut u8,
                chunk,
            )?;
            pins.push(PinnedChunk { len: chunk, pin });
            remaining -= chunk;
        }
        if remaining != 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            len,
            direction: PinnedFileIoDirection::Destination(pins),
        })
    }

    fn empty_source() -> Self {
        Self {
            len: 0,
            direction: PinnedFileIoDirection::Source(Vec::new()),
        }
    }

    fn empty_destination() -> Self {
        Self {
            len: 0,
            direction: PinnedFileIoDirection::Destination(Vec::new()),
        }
    }
}

fn checked_copy_range(total: usize, offset: usize, len: usize) -> VfsResult<()> {
    offset
        .checked_add(len)
        .filter(|&end| end <= total)
        .map(|_| ())
        .ok_or(VfsError::InvalidInput)
}

fn copy_out<T>(
    chunks: &[PinnedChunk<T>],
    mut offset: usize,
    destination: &mut [u8],
    segments: impl Fn(&T) -> &[UserIoPinSegment],
) -> VfsResult<usize> {
    let mut copied = 0usize;
    for chunk in chunks {
        if offset >= chunk.len {
            offset -= chunk.len;
            continue;
        }
        let take = (chunk.len - offset).min(destination.len() - copied);
        let mut reader = PinnedPhysicalReader::new(segments(&chunk.pin), offset, take)
            .ok_or(VfsError::InvalidInput)?;
        let count = reader.read(&mut destination[copied..copied + take])?;
        copied += count;
        if count != take {
            return Ok(copied);
        }
        offset = 0;
        if copied == destination.len() {
            return Ok(copied);
        }
    }
    Ok(copied)
}

fn copy_in<T>(
    chunks: &mut [PinnedChunk<T>],
    mut offset: usize,
    source: &[u8],
    segments: impl Fn(&T) -> &[UserIoPinSegment],
) -> VfsResult<usize> {
    let mut copied = 0usize;
    for chunk in chunks {
        if offset >= chunk.len {
            offset -= chunk.len;
            continue;
        }
        let take = (chunk.len - offset).min(source.len() - copied);
        let mut writer = PinnedPhysicalWriter::new(segments(&chunk.pin), offset, take)
            .ok_or(VfsError::InvalidInput)?;
        let count = writer.write(&source[copied..copied + take])?;
        copied += count;
        if count != take {
            return Ok(copied);
        }
        offset = 0;
        if copied == source.len() {
            return Ok(copied);
        }
    }
    Ok(copied)
}

fn visit_chunks<T>(
    chunks: &[PinnedChunk<T>],
    visitor: &mut dyn FileIoPhysicalSegmentVisitor,
    segments: impl Fn(&T) -> &[UserIoPinSegment],
) -> VfsResult<()> {
    for chunk in chunks {
        let mut remaining = chunk.len;
        for segment in segments(&chunk.pin) {
            if remaining == 0 {
                break;
            }
            let len = segment.len.min(remaining);
            if len == 0 {
                continue;
            }
            segment
                .paddr
                .checked_add(len)
                .ok_or(VfsError::InvalidInput)?;
            visitor.segment(FileIoPhysicalSegment {
                paddr: segment.paddr,
                len,
            })?;
            remaining -= len;
        }
        if remaining != 0 {
            return Err(VfsError::InvalidInput);
        }
    }
    Ok(())
}

impl OwnedFileIoBuffer for OwnedPinnedFileIoBuffer {
    fn len(&self) -> usize {
        self.len
    }

    fn supports(&self, access: FileIoBufferAccess) -> bool {
        matches!(
            (&self.direction, access),
            (PinnedFileIoDirection::Source(_), FileIoBufferAccess::Source)
                | (
                    PinnedFileIoDirection::Destination(_),
                    FileIoBufferAccess::Destination
                )
        )
    }

    fn source_copy_at(&self, offset: usize, destination: &mut [u8]) -> VfsResult<usize> {
        checked_copy_range(self.len, offset, destination.len())?;
        if destination.is_empty() {
            return Ok(0);
        }
        match &self.direction {
            PinnedFileIoDirection::Source(chunks) => {
                copy_out(chunks, offset, destination, PinnedUserSegments::segments)
            }
            PinnedFileIoDirection::Destination(_) => Err(VfsError::InvalidInput),
        }
    }

    fn destination_copy_at(&mut self, offset: usize, source: &[u8]) -> VfsResult<usize> {
        checked_copy_range(self.len, offset, source.len())?;
        if source.is_empty() {
            return Ok(0);
        }
        match &mut self.direction {
            PinnedFileIoDirection::Source(_) => Err(VfsError::InvalidInput),
            PinnedFileIoDirection::Destination(chunks) => {
                copy_in(chunks, offset, source, PinnedUserSegmentsMut::segments)
            }
        }
    }

    fn visit_physical_segments(
        &self,
        visitor: &mut dyn FileIoPhysicalSegmentVisitor,
    ) -> VfsResult<()> {
        match &self.direction {
            PinnedFileIoDirection::Source(chunks) => {
                visit_chunks(chunks, visitor, PinnedUserSegments::segments)
            }
            PinnedFileIoDirection::Destination(chunks) => {
                visit_chunks(chunks, visitor, PinnedUserSegmentsMut::segments)
            }
        }
    }
}
