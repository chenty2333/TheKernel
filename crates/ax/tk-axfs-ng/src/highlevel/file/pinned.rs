//! Pinned physical segments and cursors for zero-copy I/O.

use super::*;

/// One physically contiguous segment held stable by the caller for an I/O.
///
/// This type is only a descriptor. Dereferencing it is restricted to the
/// unsafe pinned-segment I/O methods below, whose contracts require the range
/// to remain pinned and accessible for the complete operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinnedPhysicalSegment {
    pub(super) paddr: usize,
    pub(super) len: usize,
}

impl PinnedPhysicalSegment {
    pub const fn new(paddr: usize, len: usize) -> Self {
        Self { paddr, len }
    }

    pub const fn paddr(self) -> usize {
        self.paddr
    }

    pub const fn len(self) -> usize {
        self.len
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

pub(super) const MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS: usize = 64;
pub(super) const MAX_PHYSICAL_IO_SEGMENTS: usize = 64;
pub(super) const PHYSICAL_IO_ALIGNMENT: usize = 512;

#[cfg(target_os = "none")]
pub(super) fn physical_to_virtual(paddr: PhysAddr) -> VirtAddr {
    phys_to_virt(paddr)
}

#[cfg(not(target_os = "none"))]
pub(super) fn physical_to_virtual(paddr: PhysAddr) -> VirtAddr {
    VirtAddr::from(usize::from(paddr))
}

#[cfg(target_os = "none")]
pub(super) fn virtual_to_physical(vaddr: VirtAddr) -> PhysAddr {
    virt_to_phys(vaddr)
}

#[cfg(not(target_os = "none"))]
pub(super) fn virtual_to_physical(vaddr: VirtAddr) -> PhysAddr {
    PhysAddr::from(usize::from(vaddr))
}

pub(super) fn validate_pinned_physical_segments(
    segments: &[PinnedPhysicalSegment],
    mutable: bool,
) -> VfsResult<usize> {
    if mutable && segments.len() > MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS {
        return Err(VfsError::InvalidInput);
    }
    let mut total = 0usize;
    let mut ranges = [(0usize, 0usize); MAX_MUTABLE_PINNED_PHYSICAL_SEGMENTS];
    let mut ranges_len = 0usize;
    for segment in segments.iter().copied() {
        let end = segment
            .paddr
            .checked_add(segment.len)
            .ok_or(VfsError::InvalidInput)?;
        total = total
            .checked_add(segment.len)
            .ok_or(VfsError::InvalidInput)?;
        if mutable && segment.len != 0 {
            ranges[ranges_len] = (segment.paddr, end);
            ranges_len += 1;
        }
    }
    if mutable {
        let ranges = &mut ranges[..ranges_len];
        ranges.sort_unstable_by_key(|range| range.0);
        if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(VfsError::InvalidInput);
        }
    }
    Ok(total)
}

/// Validates a physical SG request before any cache or device state is
/// touched.  Physical direct I/O is intentionally stricter than the
/// bounce-buffer pinned path: each descriptor and the complete request are
/// non-empty, checked, disjoint, and at least 512-byte aligned.
pub(super) fn validate_physical_io_segments(
    segments: &[PhysicalIoSegment],
    offset: u64,
) -> VfsResult<usize> {
    if segments.is_empty() || segments.len() > MAX_PHYSICAL_IO_SEGMENTS {
        return Err(VfsError::InvalidInput);
    }
    if offset % PHYSICAL_IO_ALIGNMENT as u64 != 0 {
        return Err(VfsError::InvalidInput);
    }

    let mut total = 0usize;
    let mut ranges = [(0usize, 0usize); MAX_PHYSICAL_IO_SEGMENTS];
    for (index, segment) in segments.iter().copied().enumerate() {
        if segment.len == 0
            || segment.paddr % PHYSICAL_IO_ALIGNMENT != 0
            || segment.len % PHYSICAL_IO_ALIGNMENT != 0
        {
            return Err(VfsError::InvalidInput);
        }
        let end = segment
            .paddr
            .checked_add(segment.len)
            .ok_or(VfsError::InvalidInput)?;
        total = total
            .checked_add(segment.len)
            .ok_or(VfsError::InvalidInput)?;
        ranges[index] = (segment.paddr, end);
    }
    if total == 0 || total % PHYSICAL_IO_ALIGNMENT != 0 {
        return Err(VfsError::InvalidInput);
    }

    ranges[..segments.len()].sort_unstable_by_key(|range| range.0);
    if ranges[..segments.len()]
        .windows(2)
        .any(|pair| pair[0].1 > pair[1].0)
    {
        return Err(VfsError::InvalidInput);
    }
    Ok(total)
}

pub(super) fn try_zeroed_pinned_io_bounce(len: usize) -> VfsResult<Vec<u8>> {
    let mut bounce = Vec::new();
    bounce
        .try_reserve_exact(len)
        .map_err(|_| VfsError::NoMemory)?;
    bounce.resize(len, 0);
    Ok(bounce)
}

pub(super) struct PinnedPhysicalCursor<'a> {
    pub(super) segments: &'a [PinnedPhysicalSegment],
    pub(super) index: usize,
    pub(super) offset: usize,
}

impl<'a> PinnedPhysicalCursor<'a> {
    pub(super) fn new(segments: &'a [PinnedPhysicalSegment]) -> Self {
        Self {
            segments,
            index: 0,
            offset: 0,
        }
    }

    pub(super) fn take(&mut self, limit: usize) -> Option<(usize, usize)> {
        while let Some(segment) = self.segments.get(self.index).copied() {
            if self.offset == segment.len {
                self.index += 1;
                self.offset = 0;
                continue;
            }
            let len = limit.min(segment.len - self.offset);
            let paddr = segment.paddr + self.offset;
            self.offset += len;
            return Some((paddr, len));
        }
        None
    }
}

pub(super) unsafe fn copy_from_pinned_physical_segments(
    cursor: &mut PinnedPhysicalCursor<'_>,
    dst: &mut [u8],
) -> VfsResult<()> {
    let mut copied = 0usize;
    while copied < dst.len() {
        let (paddr, len) = cursor
            .take(dst.len() - copied)
            .ok_or(VfsError::InvalidInput)?;
        let src = physical_to_virtual(PhysAddr::from(paddr)).as_ptr();
        unsafe { core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr().add(copied), len) };
        copied += len;
    }
    Ok(())
}

pub(super) unsafe fn copy_to_pinned_physical_segments(
    cursor: &mut PinnedPhysicalCursor<'_>,
    src: &[u8],
) -> VfsResult<()> {
    let mut copied = 0usize;
    while copied < src.len() {
        let (paddr, len) = cursor
            .take(src.len() - copied)
            .ok_or(VfsError::InvalidInput)?;
        let dst = physical_to_virtual(PhysAddr::from(paddr)).as_mut_ptr();
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr().add(copied), dst, len) };
        copied += len;
    }
    Ok(())
}

pub(super) unsafe fn read_file_into_pinned_bounce(
    file: &FileNode,
    dst: &[PinnedPhysicalSegment],
    offset: u64,
    len: usize,
) -> VfsResult<usize> {
    let mut cursor = PinnedPhysicalCursor::new(dst);
    let mut bounce = try_zeroed_pinned_io_bounce(ALIGNED_BYPASS_CHUNK.min(len).max(1))?;
    let mut total = 0usize;
    while total < len {
        let limit = (len - total).min(bounce.len());
        let current = offset
            .checked_add(total as u64)
            .ok_or(VfsError::InvalidInput)?;
        let read = match file.read_at(&mut bounce[..limit], current) {
            Ok(read) => read,
            Err(_) if total != 0 => break,
            Err(error) => return Err(error),
        };
        crate::account_backing_read(read);
        if read == 0 {
            break;
        }
        unsafe { copy_to_pinned_physical_segments(&mut cursor, &bounce[..read])? };
        total += read;
        if read < limit {
            break;
        }
    }
    Ok(total)
}

pub(super) unsafe fn write_file_from_pinned_bounce(
    file: &FileNode,
    src: &[PinnedPhysicalSegment],
    offset: u64,
    len: usize,
) -> VfsResult<usize> {
    let mut cursor = PinnedPhysicalCursor::new(src);
    let mut bounce = try_zeroed_pinned_io_bounce(ALIGNED_BYPASS_CHUNK.min(len).max(1))?;
    let mut total = 0usize;
    while total < len {
        let limit = (len - total).min(bounce.len());
        unsafe { copy_from_pinned_physical_segments(&mut cursor, &mut bounce[..limit])? };
        let current = offset
            .checked_add(total as u64)
            .ok_or(VfsError::InvalidInput)?;
        let written = match file.write_at(&bounce[..limit], current) {
            Ok(written) => written,
            Err(_) if total != 0 => break,
            Err(error) => return Err(error),
        };
        crate::account_backing_write(written);
        if written == 0 {
            break;
        }
        total += written;
        if written < limit {
            break;
        }
    }
    Ok(total)
}
