use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use axfs::FileBackend;
use axhal::paging::{MappingFlags, PageSize};
use axtask::current;
use linux_raw_sys::general::*;
use memory_addr::{MemoryAddr, VirtAddr, VirtAddrRange, align_up_4k};

use crate::{
    file::{File, FileLike},
    mm::{AddrSpace, Backend, BackendOps, SharedPages},
    pseudofs::{Device, DeviceMmap},
    task::AsThread,
};

bitflags::bitflags! {
    /// `PROT_*` flags for use with [`sys_mmap`].
    ///
    /// For `PROT_NONE`, use `ProtFlags::empty()`.
    #[derive(Debug, Clone, Copy)]
    struct MmapProt: u32 {
        /// Page can be read.
        const READ = PROT_READ;
        /// Page can be written.
        const WRITE = PROT_WRITE;
        /// Page can be executed.
        const EXEC = PROT_EXEC;
        /// Extend change to start of growsdown vma (mprotect only).
        const GROWDOWN = PROT_GROWSDOWN;
        /// Extend change to start of growsup vma (mprotect only).
        const GROWSUP = PROT_GROWSUP;
    }
}

#[derive(Clone)]
struct RemapSegment {
    start: VirtAddr,
    size: usize,
    flags: MappingFlags,
    backend: Backend,
}

fn collect_remap_segments(
    aspace: &AddrSpace,
    start: VirtAddr,
    size: usize,
) -> AxResult<Vec<RemapSegment>> {
    let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
    let mut cursor = start;
    let mut segments: Vec<RemapSegment> = Vec::new();

    while cursor < end {
        let area = aspace.find_area(cursor).ok_or(AxError::BadAddress)?;
        if area.start() > cursor {
            return Err(AxError::BadAddress);
        }

        let page_size = area.backend().page_size();
        if !cursor.is_aligned(page_size) {
            return Err(AxError::InvalidInput);
        }

        let seg_end = area.end().min(end);
        let seg_size = seg_end.sub_addr(cursor);
        if !page_size.is_aligned(seg_size) {
            return Err(AxError::InvalidInput);
        }

        if let Some(first) = segments.first()
            && (area.flags() != first.flags || !area.backend().compatible_with(&first.backend))
        {
            return Err(AxError::BadAddress);
        }

        segments.push(RemapSegment {
            start: cursor,
            size: seg_size,
            flags: area.flags(),
            backend: area.backend().clone(),
        });
        cursor = seg_end;
    }

    Ok(segments)
}

fn prefix_segments(segments: &[RemapSegment], size: usize) -> Vec<RemapSegment> {
    let mut remaining = size;
    let mut prefix = Vec::new();

    for seg in segments {
        if remaining == 0 {
            break;
        }
        let take = seg.size.min(remaining);
        prefix.push(RemapSegment {
            start: seg.start,
            size: take,
            flags: seg.flags,
            backend: seg.backend.clone(),
        });
        remaining -= take;
    }

    prefix
}

fn range_is_free(aspace: &AddrSpace, start: VirtAddr, size: usize, align: usize) -> bool {
    let Some(limit) = VirtAddrRange::try_from_start_size(start, size) else {
        return false;
    };
    aspace.find_free_area(start, size, limit, align) == Some(start)
}

fn validate_fixed_remap_dst(
    aspace: &AddrSpace,
    src: VirtAddr,
    src_size: usize,
    dst: VirtAddr,
    dst_size: usize,
) -> AxResult<()> {
    let src_range =
        VirtAddrRange::try_from_start_size(src, src_size).ok_or(AxError::InvalidInput)?;
    let dst_range =
        VirtAddrRange::try_from_start_size(dst, dst_size).ok_or(AxError::InvalidInput)?;
    if src_range.overlaps(dst_range) {
        return Err(AxError::InvalidInput);
    }
    if !aspace.contains_range(dst, dst_size) {
        return Err(AxError::NoMemory);
    }
    Ok(())
}

fn map_relocated_segments(
    aspace: &mut AddrSpace,
    aspace_handle: &Arc<axsync::Mutex<AddrSpace>>,
    old_start: VirtAddr,
    new_start: VirtAddr,
    segments: &[RemapSegment],
    preserve_mapping_identity: bool,
) -> AxResult {
    for seg in segments {
        let seg_start = new_start + seg.start.sub_addr(old_start);
        let relocated = if preserve_mapping_identity {
            seg.backend.relocate(seg.start, seg_start, aspace_handle)?
        } else {
            seg.backend
                .duplicate_mapping(seg.start, seg_start, aspace_handle)?
        };
        aspace.map(seg_start, seg.size, seg.flags, false, relocated)?;
        seg.backend.migrate_present_pages(
            seg.start,
            seg_start,
            seg.size,
            &mut aspace.page_table_mut().cursor(),
        )?;
    }
    Ok(())
}

impl From<MmapProt> for MappingFlags {
    fn from(value: MmapProt) -> Self {
        let mut flags = MappingFlags::USER;
        if value.contains(MmapProt::READ) {
            flags |= MappingFlags::READ;
        }
        if value.contains(MmapProt::WRITE) {
            flags |= MappingFlags::WRITE;
        }
        if value.contains(MmapProt::EXEC) {
            flags |= MappingFlags::EXECUTE;
        }
        flags
    }
}

bitflags::bitflags! {
    /// flags for sys_mmap
    ///
    /// See <https://github.com/bminor/glibc/blob/master/bits/mman.h>
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    struct MmapFlags: u32 {
        /// Share changes
        const SHARED = MAP_SHARED;
        /// Share changes, but fail if mapping flags contain unknown
        const SHARED_VALIDATE = MAP_SHARED_VALIDATE;
        /// Changes private; copy pages on write.
        const PRIVATE = MAP_PRIVATE;
        /// Map address must be exactly as requested, no matter whether it is available.
        const FIXED = MAP_FIXED;
        /// Same as `FIXED`, but if the requested address overlaps an existing
        /// mapping, the call fails instead of replacing the existing mapping.
        const FIXED_NOREPLACE = MAP_FIXED_NOREPLACE;
        /// Don't use a file.
        const ANONYMOUS = MAP_ANONYMOUS;
        /// Populate the mapping.
        const POPULATE = MAP_POPULATE;
        /// Don't check for reservations.
        const NORESERVE = MAP_NORESERVE;
        /// Allocation is for a stack.
        const STACK = MAP_STACK;
        /// Huge page
        const HUGE = MAP_HUGETLB;
        /// Huge page 1g size
        const HUGE_1GB = MAP_HUGETLB | MAP_HUGE_1GB;
        /// Deprecated flag
        const DENYWRITE = MAP_DENYWRITE;

        /// Mask for type of mapping
        const TYPE = MAP_TYPE;
    }
}

pub fn sys_mmap(
    addr: usize,
    length: usize,
    prot: u32,
    flags: u32,
    fd: i32,
    offset: isize,
) -> AxResult<isize> {
    if length == 0 {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let mut aspace = curr.as_thread().proc_data.aspace.lock();
    let permission_flags = MmapProt::from_bits_truncate(prot);
    // TODO: check illegal flags for mmap
    let map_flags = match MmapFlags::from_bits(flags) {
        Some(flags) => flags,
        None => {
            warn!("unknown mmap flags: {flags}");
            if (flags & MmapFlags::TYPE.bits()) == MmapFlags::SHARED_VALIDATE.bits() {
                return Err(AxError::OperationNotSupported);
            }
            MmapFlags::from_bits_truncate(flags)
        }
    };
    let map_type = map_flags & MmapFlags::TYPE;
    if !matches!(
        map_type,
        MmapFlags::PRIVATE | MmapFlags::SHARED | MmapFlags::SHARED_VALIDATE
    ) {
        return Err(AxError::InvalidInput);
    }
    if map_flags.contains(MmapFlags::ANONYMOUS) != (fd <= 0) {
        return Err(AxError::InvalidInput);
    }
    if fd <= 0 && offset != 0 {
        return Err(AxError::InvalidInput);
    }
    let offset: usize = offset.try_into().map_err(|_| AxError::InvalidInput)?;
    if !PageSize::Size4K.is_aligned(offset) {
        return Err(AxError::InvalidInput);
    }

    debug!(
        "sys_mmap <= addr: {addr:#x?}, length: {length:#x?}, prot: {permission_flags:?}, flags: \
         {map_flags:?}, fd: {fd:?}, offset: {offset:?}"
    );

    let page_size = if map_flags.contains(MmapFlags::HUGE_1GB) {
        PageSize::Size1G
    } else if map_flags.contains(MmapFlags::HUGE) {
        PageSize::Size2M
    } else {
        PageSize::Size4K
    };

    let start = addr.align_down(page_size);
    let end = (addr + length).align_up(page_size);
    let mut length = end - start;

    let start = if map_flags.intersects(MmapFlags::FIXED | MmapFlags::FIXED_NOREPLACE) {
        let dst_addr = VirtAddr::from(start);
        if !map_flags.contains(MmapFlags::FIXED_NOREPLACE) {
            aspace.unmap(dst_addr, length)?;
        }
        dst_addr
    } else {
        let align = page_size as usize;
        aspace
            .find_free_area(
                VirtAddr::from(start),
                length,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                align,
            )
            .or(aspace.find_free_area(
                aspace.base(),
                length,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                align,
            ))
            .ok_or(AxError::NoMemory)?
    };

    let file = if fd > 0 {
        Some(File::from_fd(fd)?)
    } else {
        None
    };

    let backend = match map_type {
        MmapFlags::SHARED | MmapFlags::SHARED_VALIDATE => {
            if let Some(file) = file {
                let file = file.inner();
                let backend = file.backend()?.clone();
                match file.backend()?.clone() {
                    FileBackend::Cached(cache) => {
                        // TODO(mivik): file mmap page size
                        Backend::new_file(
                            start,
                            cache,
                            file.flags(),
                            offset,
                            &curr.as_thread().proc_data.aspace,
                        )
                    }
                    FileBackend::Direct(loc) => {
                        let device = loc
                            .entry()
                            .downcast::<Device>()
                            .map_err(|_| AxError::NoSuchDevice)?;

                        match device.mmap() {
                            DeviceMmap::None => {
                                return Err(AxError::NoSuchDevice);
                            }
                            DeviceMmap::ReadOnly => {
                                Backend::new_cow(start, page_size, backend, offset as u64, None)
                            }
                            DeviceMmap::Physical(mut range) => {
                                range.start += offset;
                                if range.is_empty() {
                                    return Err(AxError::InvalidInput);
                                }
                                let max_size = range.size().align_down(page_size);
                                length = length.min(max_size);
                                Backend::new_linear(start, range.start, max_size)
                            }
                            DeviceMmap::Cache(cache) => Backend::new_file(
                                start,
                                cache,
                                file.flags(),
                                offset,
                                &curr.as_thread().proc_data.aspace,
                            ),
                        }
                    }
                }
            } else {
                Backend::new_shared(start, Arc::new(SharedPages::new(length, PageSize::Size4K)?))
            }
        }
        MmapFlags::PRIVATE => {
            if let Some(file) = file {
                // Private mapping from a file
                let backend = file.inner().backend()?.clone();
                Backend::new_cow(start, page_size, backend, offset as u64, None)
            } else {
                Backend::new_alloc(start, page_size)
            }
        }
        _ => return Err(AxError::InvalidInput),
    };

    let populate = map_flags.contains(MmapFlags::POPULATE);
    aspace.map(start, length, permission_flags.into(), populate, backend)?;

    Ok(start.as_usize() as _)
}

pub fn sys_munmap(addr: usize, length: usize) -> AxResult<isize> {
    debug!("sys_munmap <= addr: {addr:#x}, length: {length:x}");
    let curr = current();
    let mut aspace = curr.as_thread().proc_data.aspace.lock();
    let length = align_up_4k(length);
    let start_addr = VirtAddr::from(addr);
    aspace.unmap(start_addr, length)?;
    Ok(0)
}

pub fn sys_mprotect(addr: usize, length: usize, prot: u32) -> AxResult<isize> {
    // TODO: implement PROT_GROWSUP & PROT_GROWSDOWN
    let Some(permission_flags) = MmapProt::from_bits(prot) else {
        return Err(AxError::InvalidInput);
    };
    debug!("sys_mprotect <= addr: {addr:#x}, length: {length:x}, prot: {permission_flags:?}");

    if permission_flags.contains(MmapProt::GROWDOWN | MmapProt::GROWSUP) {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let mut aspace = curr.as_thread().proc_data.aspace.lock();
    let length = align_up_4k(length);
    let start_addr = VirtAddr::from(addr);
    aspace.protect(start_addr, length, permission_flags.into())?;

    Ok(0)
}

pub fn sys_mremap(
    addr: usize,
    old_size: usize,
    new_size: usize,
    flags: u32,
    new_addr: usize,
) -> AxResult<isize> {
    debug!(
        "sys_mremap <= addr: {addr:#x}, old_size: {old_size:x}, new_size: {new_size:x}, flags: \
         {flags:#x}, new_addr: {new_addr:#x}"
    );

    const SUPPORTED_FLAGS: u32 = MREMAP_MAYMOVE | MREMAP_FIXED;

    if !addr.is_multiple_of(PageSize::Size4K as usize) || new_size == 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & !SUPPORTED_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }

    let may_move = flags & MREMAP_MAYMOVE != 0;
    let fixed = flags & MREMAP_FIXED != 0;

    // MREMAP_FIXED requires MREMAP_MAYMOVE.
    if fixed && !may_move {
        return Err(AxError::InvalidInput);
    }

    let addr = VirtAddr::from(addr);
    let old_size = align_up_4k(old_size);
    let new_size = align_up_4k(new_size);
    let curr = current();
    let aspace_handle = curr.as_thread().proc_data.aspace.clone();
    let mut aspace = aspace_handle.lock();

    if old_size == 0 {
        if !may_move {
            return Err(AxError::InvalidInput);
        }

        let segments = collect_remap_segments(&aspace, addr, new_size)?;
        if !segments.iter().all(|seg| seg.backend.is_shareable()) {
            return Err(AxError::InvalidInput);
        }

        let page_size = segments[0].backend.page_size();
        let dst = if fixed {
            let dst = VirtAddr::from(new_addr);
            if !dst.is_aligned(page_size) {
                return Err(AxError::InvalidInput);
            }
            validate_fixed_remap_dst(&aspace, addr, new_size, dst, new_size)?;
            aspace.unmap(dst, new_size)?;
            dst
        } else {
            aspace
                .find_free_area(
                    addr,
                    new_size,
                    VirtAddrRange::new(aspace.base(), aspace.end()),
                    page_size as usize,
                )
                .or(aspace.find_free_area(
                    aspace.base(),
                    new_size,
                    VirtAddrRange::new(aspace.base(), aspace.end()),
                    page_size as usize,
                ))
                .ok_or(AxError::NoMemory)?
        };

        if let Err(err) =
            map_relocated_segments(&mut aspace, &aspace_handle, addr, dst, &segments, false)
        {
            let _ = aspace.unmap(dst, new_size);
            return Err(err);
        }
        return Ok(dst.as_usize() as isize);
    }

    let segments = collect_remap_segments(&aspace, addr, old_size)?;
    let page_size = segments[0].backend.page_size();
    if !page_size.is_aligned(old_size) || !page_size.is_aligned(new_size) {
        return Err(AxError::InvalidInput);
    }
    if fixed && !VirtAddr::from(new_addr).is_aligned(page_size) {
        return Err(AxError::InvalidInput);
    }

    if !fixed && new_size == old_size {
        return Ok(addr.as_usize() as isize);
    }

    if !fixed && new_size < old_size {
        aspace.unmap(addr + new_size, old_size - new_size)?;
        return Ok(addr.as_usize() as isize);
    }

    let preserve_size = old_size.min(new_size);
    let moved_segments = prefix_segments(&segments, preserve_size);
    let grow = new_size.saturating_sub(old_size);
    let primary = &segments[0];

    // Try to grow in-place first.
    let after = addr + old_size;
    let can_grow_inplace =
        !fixed && new_size > old_size && range_is_free(&aspace, after, grow, page_size as usize);
    if can_grow_inplace {
        primary.backend.ensure_range_covered(addr, new_size)?;
        let tail_backend = primary.backend.relocate(addr, addr, &aspace_handle)?;
        aspace.map(after, grow, primary.flags, false, tail_backend)?;
        return Ok(addr.as_usize() as isize);
    }

    if !may_move {
        return Err(AxError::NoMemory);
    }

    let dst = if fixed {
        let dst = VirtAddr::from(new_addr);
        validate_fixed_remap_dst(&aspace, addr, old_size, dst, new_size)?;
        dst
    } else {
        aspace
            .find_free_area(
                addr,
                new_size,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                page_size as usize,
            )
            .or(aspace.find_free_area(
                aspace.base(),
                new_size,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                page_size as usize,
            ))
            .ok_or(AxError::NoMemory)?
    };

    if new_size > old_size {
        primary.backend.ensure_range_covered(addr, new_size)?;
    }
    if fixed {
        aspace.unmap(dst, new_size)?;
    }

    if let Err(err) = map_relocated_segments(
        &mut aspace,
        &aspace_handle,
        addr,
        dst,
        &moved_segments,
        true,
    ) {
        let _ = aspace.unmap(dst, preserve_size);
        return Err(err);
    }
    if new_size > old_size {
        let tail_backend = primary.backend.relocate(addr, dst, &aspace_handle)?;
        if let Err(err) = aspace.map(dst + old_size, grow, primary.flags, false, tail_backend) {
            let _ = aspace.unmap(dst, new_size);
            return Err(err);
        }
    }
    if let Err(err) = aspace.unmap(addr, old_size) {
        let _ = aspace.unmap(dst, new_size);
        return Err(err);
    }

    Ok(dst.as_usize() as isize)
}

pub fn sys_madvise(addr: usize, length: usize, advice: u32) -> AxResult<isize> {
    debug!("sys_madvise <= addr: {addr:#x}, length: {length:x}, advice: {advice:#x}");

    if !addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(AxError::InvalidInput);
    }

    match advice {
        // Hints the kernel may safely ignore.
        MADV_NORMAL | MADV_RANDOM | MADV_SEQUENTIAL | MADV_WILLNEED | MADV_FREE | MADV_DONTNEED
        | MADV_DONTFORK | MADV_DOFORK | MADV_MERGEABLE | MADV_UNMERGEABLE | MADV_HUGEPAGE
        | MADV_NOHUGEPAGE | MADV_DONTDUMP | MADV_DODUMP | MADV_WIPEONFORK | MADV_KEEPONFORK
        | MADV_COLD | MADV_PAGEOUT | MADV_POPULATE_READ | MADV_POPULATE_WRITE | MADV_COLLAPSE => {
            Ok(0)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_msync(addr: usize, length: usize, flags: u32) -> AxResult<isize> {
    debug!("sys_msync <= addr: {addr:#x}, length: {length:x}, flags: {flags:#x}");

    if !addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(AxError::InvalidInput);
    }
    if flags & !(MS_ASYNC | MS_SYNC | MS_INVALIDATE) != 0 {
        return Err(AxError::InvalidInput);
    }

    // MS_ASYNC and MS_SYNC are mutually exclusive.
    if flags & MS_ASYNC != 0 && flags & MS_SYNC != 0 {
        return Err(AxError::InvalidInput);
    }

    // Validate the range is mapped.
    let curr = current();
    let aspace = curr.as_thread().proc_data.aspace.lock();
    let length = align_up_4k(length);
    if length > 0 && !aspace.can_access_range(VirtAddr::from(addr), length, MappingFlags::empty()) {
        return Err(AxError::NoMemory);
    }

    // No persistent backing store — sync is a no-op.
    Ok(0)
}

pub fn sys_mlock(addr: usize, length: usize) -> AxResult<isize> {
    sys_mlock2(addr, length, 0)
}

pub fn sys_mlock2(addr: usize, length: usize, flags: u32) -> AxResult<isize> {
    debug!("sys_mlock2 <= addr: {addr:#x}, length: {length:x}, flags: {flags:#x}");

    if flags & !MLOCK_ONFAULT != 0 {
        return Err(AxError::InvalidInput);
    }

    // Validate the range is mapped.
    let curr = current();
    let aspace = curr.as_thread().proc_data.aspace.lock();
    let start = VirtAddr::from(addr).align_down_4k();
    let end = VirtAddr::from(addr)
        .checked_add(length)
        .ok_or(AxError::InvalidInput)?
        .align_up_4k();
    let length = end.sub_addr(start);
    if length > 0 && !aspace.can_access_range(start, length, MappingFlags::empty()) {
        return Err(AxError::NoMemory);
    }

    // No swap — all pages are always resident. Nothing to do.
    Ok(0)
}
