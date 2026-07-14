use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FileBackend, FileFlags};
use axhal::paging::{MappingFlags, PageSize};
use axtask::current;
use linux_raw_sys::general::*;
use memory_addr::{MemoryAddr, VirtAddr, VirtAddrRange};

use crate::{
    file::{File, FileLike, get_file_description},
    mm::{
        AddrSpace, Backend, BackendOps, SharedPages, check_memory_overcommit, checked_align_up,
        checked_align_up_4k,
    },
    pseudofs::{Device, DeviceMmap},
    task::{AsThread, ProcessData},
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

fn validate_page_aligned_range(addr: usize, length: usize) -> AxResult<(VirtAddr, usize)> {
    let start = VirtAddr::from(addr).align_down_4k();
    if length == 0 {
        return Ok((start, 0));
    }
    let end = addr
        .checked_add(length)
        .and_then(checked_align_up_4k)
        .ok_or(AxError::InvalidInput)?;
    let length = end
        .checked_sub(start.as_usize())
        .ok_or(AxError::InvalidInput)?;
    Ok((start, length))
}

fn validate_file_mmap_access(
    file: &axfs::File,
    backend: &FileBackend,
    map_type: MmapFlags,
    permission_flags: MmapProt,
) -> AxResult {
    let open_flags = file.flags();
    let is_device = matches!(
        backend,
        FileBackend::Direct(loc) if loc.entry().downcast::<Device>().is_ok()
    );

    if !is_device && !open_flags.contains(FileFlags::READ) {
        return Err(AxError::PermissionDenied);
    }

    if (map_type == MmapFlags::SHARED || map_type == MmapFlags::SHARED_VALIDATE)
        && permission_flags.contains(MmapProt::WRITE)
        && !open_flags.contains(FileFlags::WRITE)
    {
        return Err(AxError::PermissionDenied);
    }

    Ok(())
}

fn may_protect_from_file_flags(open_flags: FileFlags) -> MappingFlags {
    let mut flags = MappingFlags::READ | MappingFlags::EXECUTE;
    if open_flags.contains(FileFlags::WRITE) {
        flags |= MappingFlags::WRITE;
    }
    flags
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

#[derive(Clone, Copy, Debug, Default)]
struct MadviseRangeInfo {
    all_private_anonymous: bool,
    has_shared_mapping: bool,
}

fn inspect_madvise_range(
    aspace: &AddrSpace,
    start: VirtAddr,
    size: usize,
) -> AxResult<MadviseRangeInfo> {
    let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
    let mut cursor = start;
    let mut info = MadviseRangeInfo {
        all_private_anonymous: true,
        has_shared_mapping: false,
    };

    while cursor < end {
        let Some(area) = aspace.find_area(cursor) else {
            return Err(AxError::NoMemory);
        };
        if area.start() > cursor {
            return Err(AxError::NoMemory);
        }

        match area.backend() {
            Backend::Cow(backend) => {
                info.all_private_anonymous &= backend.is_private_anonymous();
            }
            Backend::Linear(_) | Backend::Shared(_) | Backend::File(_) => {
                info.all_private_anonymous = false;
                info.has_shared_mapping = true;
            }
        }

        cursor = area.end().min(end);
    }

    Ok(info)
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
        aspace.map_with_lock_state(seg_start, seg.size, seg.flags, false, relocated, false)?;
        seg.backend.migrate_present_pages(
            seg.start,
            seg_start,
            seg.size,
            &mut aspace.page_table_mut().cursor(),
        )?;
    }
    Ok(())
}

fn locked_segments_for_remap(
    aspace: &AddrSpace,
    old_start: VirtAddr,
    size: usize,
) -> Vec<(usize, usize)> {
    aspace
        .locked_segments_in_range(old_start, size)
        .into_iter()
        .map(|(start, size)| (start.sub_addr(old_start), size))
        .collect()
}

fn set_relocated_locked_segments(
    aspace: &mut AddrSpace,
    new_start: VirtAddr,
    segments: &[(usize, usize)],
) -> AxResult {
    for &(offset, size) in segments {
        aspace.set_locked(new_start + offset, size, true)?;
    }
    Ok(())
}

fn check_mremap_locked_growth_limit(
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    aspace: &AddrSpace,
    grow: usize,
    reclaimed_locked: usize,
) -> AxResult {
    if grow == 0 || has_ipc_lock {
        return Ok(());
    }

    let limit_error = AxError::from(LinuxError::EAGAIN);
    let locked_bytes = aspace
        .locked_bytes()
        .saturating_sub(reclaimed_locked)
        .checked_add(grow)
        .ok_or(limit_error)?;
    let limit = proc_data.rlim.read()[RLIMIT_MEMLOCK].current;
    if (locked_bytes as u128) > u128::from(limit) {
        return Err(limit_error);
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
        /// Stack-like mapping that may expand downward on demand.
        const GROWDOWN = MAP_GROWSDOWN;
        /// Map address must be exactly as requested, no matter whether it is available.
        const FIXED = MAP_FIXED;
        /// Same as `FIXED`, but if the requested address overlaps an existing
        /// mapping, the call fails instead of replacing the existing mapping.
        const FIXED_NOREPLACE = MAP_FIXED_NOREPLACE;
        /// Don't use a file.
        const ANONYMOUS = MAP_ANONYMOUS;
        /// Populate the mapping.
        const POPULATE = MAP_POPULATE;
        /// Lock the mapped pages, as with mlock(2).
        const LOCKED = MAP_LOCKED;
        /// Don't check for reservations.
        const NORESERVE = MAP_NORESERVE;
        /// Allocation is for a stack.
        const STACK = MAP_STACK;
        /// Huge page
        const HUGE = MAP_HUGETLB;
        /// Explicit 2 MiB huge-page size.
        const HUGE_2MB = MAP_HUGE_2MB;
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
    let permission_flags = MmapProt::from_bits(prot).ok_or(AxError::InvalidInput)?;
    if permission_flags.intersects(MmapProt::GROWDOWN | MmapProt::GROWSUP) {
        return Err(AxError::InvalidInput);
    }
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
    let is_anonymous_mapping = map_flags.contains(MmapFlags::ANONYMOUS);
    if map_flags.contains(MmapFlags::GROWDOWN)
        && (map_type != MmapFlags::PRIVATE
            || !is_anonymous_mapping
            || map_flags.contains(MmapFlags::HUGE))
    {
        return Err(AxError::OperationNotSupported);
    }
    if map_flags.contains(MmapFlags::HUGE) && !is_anonymous_mapping {
        return Err(AxError::InvalidInput);
    }
    if is_anonymous_mapping && offset != 0 {
        return Err(AxError::InvalidInput);
    }
    if !is_anonymous_mapping {
        if fd < 0 {
            return Err(AxError::BadFileDescriptor);
        }
        get_file_description(fd).map_err(|_| AxError::BadFileDescriptor)?;
    }
    if length == 0 {
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

    let huge_page_order = (flags >> MAP_HUGE_SHIFT) & MAP_HUGE_MASK;
    let page_size = if map_flags.contains(MmapFlags::HUGE) {
        match huge_page_order {
            0 | 21 => PageSize::Size2M,
            30 => PageSize::Size1G,
            _ => return Err(AxError::InvalidInput),
        }
    } else {
        PageSize::Size4K
    };
    if map_flags.intersects(MmapFlags::FIXED | MmapFlags::FIXED_NOREPLACE)
        && !addr.is_multiple_of(page_size as usize)
    {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    let has_ipc_lock = thread.has_effective_capability(CAP_IPC_LOCK);
    let aspace_handle = curr.as_thread().proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    let start = addr.align_down(page_size);
    let end = addr
        .checked_add(length)
        .and_then(|end| checked_align_up(end, page_size as usize))
        .ok_or(AxError::NoMemory)?;
    let mut length = end - start;

    if is_anonymous_mapping && permission_flags.contains(MmapProt::WRITE) {
        check_memory_overcommit(length)?;
    }

    let start = if map_flags.intersects(MmapFlags::FIXED | MmapFlags::FIXED_NOREPLACE) {
        let dst_addr = VirtAddr::from(start);
        if !aspace.contains_range(dst_addr, length) {
            return Err(AxError::NoMemory);
        }
        dst_addr
    } else {
        let align = page_size as usize;
        aspace
            .find_kernel_area(
                VirtAddr::from(start),
                length,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                align,
            )
            .ok_or(AxError::NoMemory)?
    };

    let file = if !is_anonymous_mapping {
        Some(File::from_fd(fd)?)
    } else {
        None
    };
    let growdown_private_anon = map_flags.contains(MmapFlags::GROWDOWN)
        && map_type == MmapFlags::PRIVATE
        && is_anonymous_mapping;

    let backend = match map_type {
        MmapFlags::SHARED | MmapFlags::SHARED_VALIDATE => {
            if let Some(file) = file {
                let file = file.inner();
                let backend = file.backend()?.clone();
                validate_file_mmap_access(file, &backend, map_type, permission_flags)?;
                match file.backend()?.clone() {
                    FileBackend::Cached(cache) => {
                        let file_end = file.location().len()?;
                        // TODO(mivik): file mmap page size
                        Backend::new_file(
                            start,
                            cache,
                            file.flags(),
                            offset,
                            Some(file_end),
                            &aspace_handle,
                        )?
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
                            DeviceMmap::Anonymous => Backend::new_shared_with_may_protect(
                                start,
                                Arc::new(SharedPages::new(length, PageSize::Size4K)?),
                                may_protect_from_file_flags(file.flags()),
                            ),
                            DeviceMmap::ReadOnly => Backend::new_cow(
                                start,
                                page_size,
                                loc.clone(),
                                offset as u64,
                                None,
                                false,
                            ),
                            DeviceMmap::Physical(mut range) => {
                                range.start += offset;
                                if range.is_empty() {
                                    return Err(AxError::InvalidInput);
                                }
                                let max_size = range.size().align_down(page_size);
                                length = length.min(max_size);
                                Backend::new_linear(start, range.start, max_size)
                            }
                            DeviceMmap::Cache(cache) => {
                                let file_end = file.location().len()?;
                                Backend::new_file(
                                    start,
                                    cache,
                                    file.flags(),
                                    offset,
                                    Some(file_end),
                                    &aspace_handle,
                                )?
                            }
                        }
                    }
                }
            } else {
                Backend::new_shared(start, Arc::new(SharedPages::new(length, page_size)?))
            }
        }
        MmapFlags::PRIVATE => {
            if let Some(file) = file {
                // Private mapping from a file
                let backend = file.inner().backend()?.clone();
                validate_file_mmap_access(file.inner(), &backend, map_type, permission_flags)?;
                match backend {
                    FileBackend::Direct(loc) => {
                        let device_mmap = loc.entry().downcast::<Device>().ok().map(|it| it.mmap());
                        match device_mmap {
                            Some(DeviceMmap::None) => return Err(AxError::NoSuchDevice),
                            Some(DeviceMmap::Anonymous) => Backend::new_alloc(start, page_size),
                            Some(DeviceMmap::Physical(mut range)) => {
                                range.start += offset;
                                if range.is_empty() {
                                    return Err(AxError::InvalidInput);
                                }
                                let max_size = range.size().align_down(page_size);
                                length = length.min(max_size);
                                Backend::new_linear(start, range.start, max_size)
                            }
                            Some(DeviceMmap::ReadOnly | DeviceMmap::Cache(_)) | None => {
                                let file_end = file.inner().location().len()?;
                                Backend::new_cow(
                                    start,
                                    page_size,
                                    file.inner().location().clone(),
                                    offset as u64,
                                    Some(file_end),
                                    true,
                                )
                            }
                        }
                    }
                    FileBackend::Cached(_) => {
                        let file_end = file.inner().location().len()?;
                        Backend::new_cow(
                            start,
                            page_size,
                            file.inner().location().clone(),
                            offset as u64,
                            Some(file_end),
                            true,
                        )
                    }
                }
            } else {
                Backend::new_alloc(start, page_size)
            }
        }
        _ => return Err(AxError::InvalidInput),
    };

    let locked_mapping = map_flags.contains(MmapFlags::LOCKED) || aspace.locks_future_mappings();
    if locked_mapping {
        check_mmap_memlock_limit(proc_data, has_ipc_lock, &aspace, start, length)?;
    }

    let populate = map_flags.contains(MmapFlags::POPULATE)
        || map_flags.contains(MmapFlags::LOCKED)
        || (aspace.locks_future_mappings()
            && !aspace.locks_future_mappings_on_fault()
            && !permission_flags.is_empty());
    if map_flags.contains(MmapFlags::FIXED) && !map_flags.contains(MmapFlags::FIXED_NOREPLACE) {
        aspace.unmap(start, length)?;
        proc_data.clear_mempolicy_range(start.as_usize(), length);
    }
    aspace.map(start, length, permission_flags.into(), populate, backend)?;
    if map_flags.contains(MmapFlags::LOCKED) {
        aspace.set_locked(start, length, true)?;
    }
    if growdown_private_anon {
        aspace.mark_growdown(start);
    }

    Ok(start.as_usize() as _)
}

pub fn sys_munmap(addr: usize, length: usize) -> AxResult<isize> {
    debug!("sys_munmap <= addr: {addr:#x}, length: {length:x}");
    const PAGE_SIZE: usize = PageSize::Size4K as usize;
    const USER_SPACE_END: usize = crate::config::USER_SPACE_BASE + crate::config::USER_SPACE_SIZE;

    if !addr.is_multiple_of(PAGE_SIZE) || length == 0 {
        return Err(AxError::InvalidInput);
    }
    if length > USER_SPACE_END.saturating_sub(addr) {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    let aspace_handle = proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    let length = checked_align_up_4k(length).ok_or(AxError::InvalidInput)?;
    let start_addr = VirtAddr::from(addr);
    aspace.unmap(start_addr, length)?;
    proc_data.clear_mempolicy_range(start_addr.as_usize(), length);
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
    let aspace_handle = curr.as_thread().proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    let length = checked_align_up_4k(length).ok_or(AxError::NoMemory)?;
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
    let old_size = checked_align_up_4k(old_size).ok_or(AxError::InvalidInput)?;
    let new_size = checked_align_up_4k(new_size).ok_or(AxError::InvalidInput)?;
    let curr = current();
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    let has_ipc_lock = thread.has_effective_capability(CAP_IPC_LOCK);
    let aspace_handle = proc_data.aspace();
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
            proc_data.clear_mempolicy_range(dst.as_usize(), new_size);
            dst
        } else {
            aspace
                .find_kernel_area(
                    addr,
                    new_size,
                    VirtAddrRange::new(aspace.base(), aspace.end()),
                    page_size as usize,
                )
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
        proc_data.clear_mempolicy_range((addr + new_size).as_usize(), old_size - new_size);
        return Ok(addr.as_usize() as isize);
    }

    let preserve_size = old_size.min(new_size);
    let moved_segments = prefix_segments(&segments, preserve_size);
    let grow = new_size.saturating_sub(old_size);
    let primary = &segments[0];
    let locked_segments = locked_segments_for_remap(&aspace, addr, preserve_size);
    let grow_locked = new_size > old_size && aspace.range_is_fully_locked(addr, old_size);

    // Try to grow in-place first.
    let after = addr + old_size;
    let can_grow_inplace =
        !fixed && new_size > old_size && range_is_free(&aspace, after, grow, page_size as usize);
    if can_grow_inplace {
        if grow_locked {
            check_mremap_locked_growth_limit(proc_data, has_ipc_lock, &aspace, grow, 0)?;
        }
        primary.backend.ensure_range_covered(addr, new_size)?;
        let tail_backend = primary.backend.relocate(addr, addr, &aspace_handle)?;
        if let Err(err) = aspace.map_with_lock_state(
            after,
            grow,
            primary.flags,
            grow_locked,
            tail_backend,
            grow_locked,
        ) {
            let _ = aspace.unmap(after, grow);
            return Err(err);
        }
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
            .find_kernel_area(
                addr,
                new_size,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                page_size as usize,
            )
            .ok_or(AxError::NoMemory)?
    };

    if new_size > old_size {
        if grow_locked {
            let reclaimed_locked = fixed
                .then(|| aspace.locked_bytes_in_range(dst, new_size))
                .unwrap_or(0);
            check_mremap_locked_growth_limit(
                proc_data,
                has_ipc_lock,
                &aspace,
                grow,
                reclaimed_locked,
            )?;
        }
        primary.backend.ensure_range_covered(addr, new_size)?;
    }
    if fixed {
        aspace.unmap(dst, new_size)?;
        proc_data.clear_mempolicy_range(dst.as_usize(), new_size);
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
        if let Err(err) = aspace.map_with_lock_state(
            dst + old_size,
            grow,
            primary.flags,
            grow_locked,
            tail_backend,
            grow_locked,
        ) {
            let _ = aspace.unmap(dst, new_size);
            return Err(err);
        }
    }
    if let Err(err) = aspace.unmap(addr, old_size) {
        let _ = aspace.unmap(dst, new_size);
        return Err(err);
    }
    proc_data.clear_mempolicy_range(addr.as_usize(), old_size);
    proc_data.clear_mempolicy_range(dst.as_usize(), new_size);
    set_relocated_locked_segments(&mut aspace, dst, &locked_segments)?;

    Ok(dst.as_usize() as isize)
}

pub fn sys_madvise(addr: usize, length: usize, advice: u32) -> AxResult<isize> {
    debug!("sys_madvise <= addr: {addr:#x}, length: {length:x}, advice: {advice:#x}");

    if !addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(AxError::InvalidInput);
    }
    if length == 0 {
        return Ok(0);
    }

    let curr = current();
    let aspace_handle = curr.as_thread().proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    let (start, length) = validate_page_aligned_range(addr, length)?;

    match advice {
        // Hints the kernel may safely ignore once the range is known-valid.
        MADV_NORMAL | MADV_RANDOM | MADV_SEQUENTIAL | MADV_WILLNEED => {
            inspect_madvise_range(&aspace, start, length)?;
            Ok(0)
        }
        MADV_DONTFORK | MADV_DOFORK => {
            inspect_madvise_range(&aspace, start, length)?;
            aspace.set_dontfork(start, length, advice == MADV_DONTFORK)?;
            Ok(0)
        }
        MADV_POPULATE_READ => {
            inspect_madvise_range(&aspace, start, length)?;
            aspace.populate_area(start, length, MappingFlags::READ)?;
            Ok(0)
        }
        MADV_POPULATE_WRITE => {
            inspect_madvise_range(&aspace, start, length)?;
            aspace.populate_area(start, length, MappingFlags::WRITE)?;
            Ok(0)
        }
        MADV_DONTNEED => {
            let info = inspect_madvise_range(&aspace, start, length)?;
            if info.has_shared_mapping || aspace.range_is_locked(start, length) {
                return Err(AxError::InvalidInput);
            }
            aspace.discard_pages(start, length)?;
            Ok(0)
        }
        MADV_FREE => {
            let info = inspect_madvise_range(&aspace, start, length)?;
            if !info.all_private_anonymous {
                return Err(AxError::InvalidInput);
            }
            Ok(0)
        }
        MADV_MERGEABLE | MADV_UNMERGEABLE | MADV_REMOVE | MADV_HUGEPAGE | MADV_NOHUGEPAGE
        | MADV_DONTDUMP | MADV_DODUMP | MADV_COLD | MADV_PAGEOUT | MADV_COLLAPSE => {
            Err(AxError::InvalidInput)
        }
        MADV_WIPEONFORK => {
            let end = start + length;
            let mut cursor = start;
            while cursor < end {
                let Some(area) = aspace.find_area(cursor) else {
                    return Err(AxError::NoMemory);
                };
                if area.start() > cursor {
                    return Err(AxError::NoMemory);
                }
                if !area.backend().is_private_anonymous() {
                    return Err(AxError::InvalidInput);
                }
                cursor = area.end().min(end);
            }
            aspace.set_wipe_on_fork(start, length, advice == MADV_WIPEONFORK)?;
            Ok(0)
        }
        MADV_KEEPONFORK => {
            inspect_madvise_range(&aspace, start, length)?;
            aspace.set_wipe_on_fork(start, length, false)?;
            Ok(0)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_msync(addr: usize, length: usize, flags: u32) -> AxResult<isize> {
    debug!("sys_msync <= addr: {addr:#x}, length: {length:x}, flags: {flags:#x}");
    const PAGE_SIZE: usize = PageSize::Size4K as usize;

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

    // Validate the range while holding the address-space lock, then sync
    // outside the lock so page-cache eviction callbacks can unmap old PTEs.
    let curr = current();
    let aspace_handle = curr.as_thread().proc_data.aspace();
    let length = checked_align_up(length, PAGE_SIZE).ok_or(AxError::NoMemory)?;
    addr.checked_add(length).ok_or(AxError::NoMemory)?;
    let fail_on_first_unmapped = flags == MS_ASYNC;
    let (backends, saw_unmapped) = {
        let aspace = aspace_handle.lock();
        if length > 0 {
            let start = VirtAddr::from(addr);
            let (backends, saw_unmapped) =
                aspace.sync_backends_in_range(start, length, fail_on_first_unmapped)?;
            if flags & MS_INVALIDATE != 0 && aspace.range_is_locked(start, length) {
                return Err(LinuxError::EBUSY.into());
            }
            (backends, saw_unmapped)
        } else {
            (Vec::new(), false)
        }
    };

    if flags & MS_SYNC != 0 {
        for backend in backends {
            backend.sync(false)?;
        }
    }

    if saw_unmapped {
        return Err(AxError::NoMemory);
    }

    Ok(0)
}

pub fn sys_mlock(addr: usize, length: usize) -> AxResult<isize> {
    sys_mlock2(addr, length, 0)
}

fn memlock_limit(proc_data: &ProcessData, has_ipc_lock: bool) -> AxResult<Option<u64>> {
    if has_ipc_lock {
        return Ok(None);
    }
    let limit = proc_data.rlim.read()[RLIMIT_MEMLOCK].current;
    if limit == 0 {
        return Err(AxError::OperationNotPermitted);
    }

    Ok(Some(limit))
}

fn check_memlock_total(
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    locked_bytes: usize,
    limit_error: AxError,
) -> AxResult {
    if let Some(limit) = memlock_limit(proc_data, has_ipc_lock)?
        && (locked_bytes as u128) > u128::from(limit)
    {
        return Err(limit_error);
    }

    Ok(())
}

fn locked_bytes_after_range(
    aspace: &AddrSpace,
    start: VirtAddr,
    length: usize,
    overflow_error: AxError,
) -> AxResult<usize> {
    let already_locked = aspace.locked_bytes_in_range(start, length);
    let additional_locked = length.saturating_sub(already_locked);
    aspace
        .locked_bytes()
        .checked_add(additional_locked)
        .ok_or(overflow_error)
}

fn check_mlock_range_limit(
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    aspace: &AddrSpace,
    start: VirtAddr,
    length: usize,
) -> AxResult {
    let locked_bytes = locked_bytes_after_range(aspace, start, length, AxError::NoMemory)?;
    check_memlock_total(proc_data, has_ipc_lock, locked_bytes, AxError::NoMemory)
}

pub(super) fn check_mmap_memlock_limit(
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    aspace: &AddrSpace,
    start: VirtAddr,
    length: usize,
) -> AxResult {
    let limit_error = AxError::from(LinuxError::EAGAIN);
    let locked_bytes = locked_bytes_after_range(aspace, start, length, limit_error)?;
    check_memlock_total(proc_data, has_ipc_lock, locked_bytes, limit_error)
}

fn check_mlockall_current_limit(
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    aspace: &AddrSpace,
) -> AxResult {
    check_memlock_total(
        proc_data,
        has_ipc_lock,
        aspace.current_mapping_bytes(),
        AxError::NoMemory,
    )
}

fn check_mlockall_future_limit(proc_data: &ProcessData, has_ipc_lock: bool) -> AxResult {
    memlock_limit(proc_data, has_ipc_lock).map(|_| ())
}

pub fn sys_mlock2(addr: usize, length: usize, flags: u32) -> AxResult<isize> {
    debug!("sys_mlock2 <= addr: {addr:#x}, length: {length:x}, flags: {flags:#x}");

    if flags & !MLOCK_ONFAULT != 0 {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    let has_ipc_lock = thread.has_effective_capability(CAP_IPC_LOCK);
    let aspace_handle = proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    let (start, length) = validate_page_aligned_range(addr, length)?;
    if length > 0 && !aspace.can_access_range(start, length, MappingFlags::empty()) {
        return Err(AxError::NoMemory);
    }

    check_mlock_range_limit(proc_data, has_ipc_lock, &aspace, start, length)?;
    if flags & MLOCK_ONFAULT == 0 {
        aspace.populate_area(start, length, MappingFlags::empty())?;
    }

    aspace.set_locked(start, length, true)?;
    Ok(0)
}

pub fn sys_munlock(addr: usize, length: usize) -> AxResult<isize> {
    debug!("sys_munlock <= addr: {addr:#x}, length: {length:x}");

    let curr = current();
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    let aspace_handle = proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    let (start, length) = validate_page_aligned_range(addr, length)?;
    if length > 0 && !aspace.can_access_range(start, length, MappingFlags::empty()) {
        return Err(AxError::NoMemory);
    }

    aspace.set_locked(start, length, false)?;
    Ok(0)
}

pub fn sys_mlockall(flags: u32) -> AxResult<isize> {
    debug!("sys_mlockall <= flags: {flags:#x}");

    const MCL_LOCK_FLAGS: u32 = MCL_CURRENT | MCL_FUTURE;
    const MCL_SUPPORTED_FLAGS: u32 = MCL_LOCK_FLAGS | MCL_ONFAULT;

    if flags & !MCL_SUPPORTED_FLAGS != 0 || flags & MCL_LOCK_FLAGS == 0 {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    let has_ipc_lock = thread.has_effective_capability(CAP_IPC_LOCK);
    let aspace_handle = proc_data.aspace();
    let mut aspace = aspace_handle.lock();

    if flags & MCL_CURRENT != 0 {
        check_mlockall_current_limit(proc_data, has_ipc_lock, &aspace)?;
        if flags & MCL_ONFAULT == 0 {
            let ranges: Vec<_> = aspace
                .areas()
                .map(|area| (area.start(), area.size()))
                .collect();
            for (start, size) in ranges {
                aspace.populate_area(start, size, MappingFlags::empty())?;
            }
        }
        aspace.lock_current_mappings();
    } else {
        check_mlockall_future_limit(proc_data, has_ipc_lock)?;
    }

    aspace.set_lock_future_mappings(flags & MCL_FUTURE != 0, flags & MCL_ONFAULT != 0);

    Ok(0)
}

pub fn sys_munlockall() -> AxResult<isize> {
    debug!("sys_munlockall");

    let curr = current();
    let aspace_handle = curr.as_thread().proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    aspace.clear_locked_mappings();

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_does_not_reduce_mprotect_write_capability() {
        let open_flags = FileFlags::READ | FileFlags::WRITE | FileFlags::APPEND;
        assert!(may_protect_from_file_flags(open_flags).contains(MappingFlags::WRITE));
    }
}
