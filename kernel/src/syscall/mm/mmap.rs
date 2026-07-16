use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{CachedFile, FileBackend, FileFlags};
use axfs_ng_vfs::Location;
use axhal::paging::{MappingFlags, PageSize};
use axtask::current;
use linux_raw_sys::general::*;
use memory_addr::{MemoryAddr, VirtAddr, VirtAddrRange};

use crate::{
    file::{
        Directory, File, FileHandle, FileMmapProtection, FileMmapRequest, FileMmapSharing,
        executable, get_file_like,
        privilege_metadata::{
            ContentWritePrivilegeGuard, begin_shared_writable_mapping_privilege_cleanup,
        },
    },
    mm::{
        AddrSpace, Backend, FileMappingLease, FileMappingSharing, PreparedFixedSharedMapping,
        PreparedProtect, SharedPages, WritableMappingAdmission, check_memory_overcommit,
        checked_align_up, checked_align_up_4k, remap_user_mapping,
    },
    pseudofs::{Device, DeviceMmap},
    task::{
        AsThread, ProcessData,
        security::{file_mprotect, initial_user_namespace, mmap_addr, mmap_file},
    },
};

fn lookup_mmap_fd_once<T>(fd: i32, lookup: impl FnOnce(i32) -> AxResult<T>) -> AxResult<T> {
    lookup(fd).map_err(|_| AxError::BadFileDescriptor)
}

#[cfg(test)]
fn authorize_mmap_candidate<T>(
    authorize_file: impl FnOnce() -> AxResult<()>,
    authorize_address: impl FnOnce() -> AxResult<()>,
    commit: impl FnOnce() -> AxResult<T>,
) -> AxResult<T> {
    authorize_file()?;
    authorize_address()?;
    commit()
}

fn authorize_all<T>(
    segments: impl IntoIterator<Item = T>,
    mut authorize: impl FnMut(T) -> AxResult<()>,
) -> AxResult<()> {
    for segment in segments {
        authorize(segment)?;
    }
    Ok(())
}

fn authorize_then_commit<P, T>(
    plan: P,
    authorize: impl FnOnce(&P) -> AxResult<()>,
    commit: impl FnOnce(P) -> AxResult<T>,
) -> AxResult<T> {
    authorize(&plan)?;
    commit(plan)
}

fn push_unique_content_location(
    locations: &mut Vec<(executable::ExecutableKey, Location)>,
    location: &Location,
) -> bool {
    let Some(key) = executable::key(location) else {
        return false;
    };
    if locations.iter().any(|(existing, _)| *existing == key) {
        return false;
    }
    locations.push((key, location.clone()));
    true
}

fn begin_shared_writable_protection(
    plan: &PreparedProtect<'_>,
    effective_protection: MappingFlags,
) -> AxResult<(
    Vec<WritableMappingAdmission>,
    Vec<ContentWritePrivilegeGuard>,
)> {
    if !effective_protection.contains(MappingFlags::WRITE) {
        return Ok((Vec::new(), Vec::new()));
    }

    let segment_count = plan.segments().count();
    let mut admissions = Vec::new();
    let mut locations = Vec::new();
    let mut guards = Vec::new();
    admissions
        .try_reserve(segment_count)
        .map_err(|_| AxError::NoMemory)?;
    locations
        .try_reserve(segment_count)
        .map_err(|_| AxError::NoMemory)?;
    guards
        .try_reserve(segment_count)
        .map_err(|_| AxError::NoMemory)?;

    for segment in plan.segments() {
        if segment.flags().contains(MappingFlags::WRITE) {
            continue;
        }
        let Some(mapping) = segment.file_mapping() else {
            continue;
        };
        if mapping.sharing() != FileMappingSharing::Shared {
            continue;
        }
        let Some(location) = segment.backend().shared_file_location() else {
            continue;
        };
        let Some(admission) = segment
            .backend()
            .begin_shared_writable_mapping_admission()?
        else {
            return Err(AxError::BadState);
        };
        admissions.push(admission);

        push_unique_content_location(&mut locations, location);
    }

    for (_, location) in &locations {
        guards.push(begin_shared_writable_mapping_privilege_cleanup(location)?);
    }
    Ok((admissions, guards))
}

fn commit_shared_writable_protection(
    plan: PreparedProtect<'_>,
    effective_protection: MappingFlags,
) -> AxResult<()> {
    let (admissions, guards) = begin_shared_writable_protection(&plan, effective_protection)?;
    if let Err(error) = plan.commit() {
        drop(admissions);
        drop(guards);
        return Err(error);
    }
    for admission in admissions {
        admission
            .complete()
            .expect("writable mapping admission vanished after mprotect commit");
    }
    drop(guards);
    Ok(())
}

enum PreparedFileMmapBackend {
    SharedFile {
        cache: CachedFile,
        flags: FileFlags,
        file_end: u64,
    },
    SharedAnonymous {
        may_protect: MappingFlags,
    },
    Cow {
        location: Location,
        file_end: Option<u64>,
        sigbus_on_eof: bool,
    },
    AnonymousCow,
    Linear {
        physical_start: memory_addr::PhysAddr,
        max_size: usize,
    },
}

fn prepare_file_mmap_backend(
    file: &FileHandle<File>,
    map_type: MmapFlags,
    permission_flags: MmapProt,
    offset: usize,
    page_size: PageSize,
    length: &mut usize,
) -> AxResult<PreparedFileMmapBackend> {
    let inner = file.inner();
    let file_backend = inner.backend()?.clone();
    validate_file_mmap_access(inner, &file_backend, map_type, permission_flags)?;

    match (map_type, file_backend) {
        (MmapFlags::SHARED | MmapFlags::SHARED_VALIDATE, FileBackend::Cached(cache)) => {
            Ok(PreparedFileMmapBackend::SharedFile {
                cache,
                flags: inner.flags(),
                file_end: inner.location().len()?,
            })
        }
        (MmapFlags::SHARED | MmapFlags::SHARED_VALIDATE, FileBackend::Direct(location)) => {
            let device = location
                .entry()
                .downcast::<Device>()
                .map_err(|_| AxError::NoSuchDevice)?;
            match device.mmap() {
                DeviceMmap::None => Err(AxError::NoSuchDevice),
                DeviceMmap::Anonymous => Ok(PreparedFileMmapBackend::SharedAnonymous {
                    may_protect: may_protect_from_file_flags(inner.flags()),
                }),
                DeviceMmap::ReadOnly => Ok(PreparedFileMmapBackend::Cow {
                    location,
                    file_end: None,
                    sigbus_on_eof: false,
                }),
                DeviceMmap::Physical(mut range) => {
                    range.start += offset;
                    if range.is_empty() {
                        return Err(AxError::InvalidInput);
                    }
                    let max_size = range.size().align_down(page_size);
                    *length = (*length).min(max_size);
                    Ok(PreparedFileMmapBackend::Linear {
                        physical_start: range.start,
                        max_size,
                    })
                }
                DeviceMmap::Cache(cache) => Ok(PreparedFileMmapBackend::SharedFile {
                    cache,
                    flags: inner.flags(),
                    file_end: inner.location().len()?,
                }),
            }
        }
        (MmapFlags::PRIVATE, FileBackend::Direct(location)) => {
            match location
                .entry()
                .downcast::<Device>()
                .ok()
                .map(|device| device.mmap())
            {
                Some(DeviceMmap::None) => Err(AxError::NoSuchDevice),
                Some(DeviceMmap::Anonymous) => Ok(PreparedFileMmapBackend::AnonymousCow),
                Some(DeviceMmap::Physical(mut range)) => {
                    range.start += offset;
                    if range.is_empty() {
                        return Err(AxError::InvalidInput);
                    }
                    let max_size = range.size().align_down(page_size);
                    *length = (*length).min(max_size);
                    Ok(PreparedFileMmapBackend::Linear {
                        physical_start: range.start,
                        max_size,
                    })
                }
                Some(DeviceMmap::ReadOnly | DeviceMmap::Cache(_)) | None => {
                    Ok(PreparedFileMmapBackend::Cow {
                        file_end: Some(inner.location().len()?),
                        location,
                        sigbus_on_eof: true,
                    })
                }
            }
        }
        (MmapFlags::PRIVATE, FileBackend::Cached(_)) => Ok(PreparedFileMmapBackend::Cow {
            location: inner.location().clone(),
            file_end: Some(inner.location().len()?),
            sigbus_on_eof: true,
        }),
        _ => Err(AxError::InvalidInput),
    }
}

bitflags::bitflags! {
    /// `PROT_*` flags for use with [`sys_mmap`].
    ///
    /// For `PROT_NONE`, use `ProtFlags::empty()`.
    #[derive(Debug, Clone, Copy)]
    struct MmapProt: usize {
        /// Page can be read.
        const READ = PROT_READ as usize;
        /// Page can be written.
        const WRITE = PROT_WRITE as usize;
        /// Page can be executed.
        const EXEC = PROT_EXEC as usize;
        /// Extend change to start of growsdown vma (mprotect only).
        const GROWDOWN = PROT_GROWSDOWN as usize;
        /// Extend change to start of growsup vma (mprotect only).
        const GROWSUP = PROT_GROWSUP as usize;
    }
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

#[derive(Clone, Copy, Debug, Default)]
struct MadviseRangeInfo {
    all_private_anonymous: bool,
    has_shared_mapping: bool,
}

fn classify_madvise_backend(backend: &Backend) -> MadviseRangeInfo {
    MadviseRangeInfo {
        all_private_anonymous: backend.is_private_anonymous(),
        has_shared_mapping: matches!(
            backend,
            Backend::Linear(_) | Backend::Shared(_) | Backend::File(_)
        ),
    }
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

        let area_info = classify_madvise_backend(area.backend());
        info.all_private_anonymous &= area_info.all_private_anonymous;
        info.has_shared_mapping |= area_info.has_shared_mapping;

        cursor = area.end().min(end);
    }

    Ok(info)
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
    struct MmapFlags: usize {
        /// Share changes
        const SHARED = MAP_SHARED as usize;
        /// Share changes, but fail if mapping flags contain unknown
        const SHARED_VALIDATE = MAP_SHARED_VALIDATE as usize;
        /// Changes private; copy pages on write.
        const PRIVATE = MAP_PRIVATE as usize;
        /// Stack-like mapping that may expand downward on demand.
        const GROWDOWN = MAP_GROWSDOWN as usize;
        /// Map address must be exactly as requested, no matter whether it is available.
        const FIXED = MAP_FIXED as usize;
        /// Same as `FIXED`, but if the requested address overlaps an existing
        /// mapping, the call fails instead of replacing the existing mapping.
        const FIXED_NOREPLACE = MAP_FIXED_NOREPLACE as usize;
        /// Don't use a file.
        const ANONYMOUS = MAP_ANONYMOUS as usize;
        /// Populate the mapping.
        const POPULATE = MAP_POPULATE as usize;
        /// Lock the mapped pages, as with mlock(2).
        const LOCKED = MAP_LOCKED as usize;
        /// Don't check for reservations.
        const NORESERVE = MAP_NORESERVE as usize;
        /// Allocation is for a stack.
        const STACK = MAP_STACK as usize;
        /// Huge page
        const HUGE = MAP_HUGETLB as usize;
        /// Explicit 2 MiB huge-page size.
        const HUGE_2MB = MAP_HUGE_2MB as usize;
        /// Huge page 1g size
        const HUGE_1GB = (MAP_HUGETLB | MAP_HUGE_1GB) as usize;
        /// Deprecated flag
        const DENYWRITE = MAP_DENYWRITE as usize;

        /// Mask for type of mapping
        const TYPE = MAP_TYPE as usize;
    }
}

pub fn sys_mmap(
    addr: usize,
    length: usize,
    prot: usize,
    flags: usize,
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
    let pinned_fd = if !is_anonymous_mapping {
        if fd < 0 {
            return Err(AxError::BadFileDescriptor);
        }
        Some(lookup_mmap_fd_once(fd, get_file_like)?)
    } else {
        None
    };
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

    let huge_page_order = (flags >> MAP_HUGE_SHIFT) & MAP_HUGE_MASK as usize;
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

    let normalized_start = addr.align_down(page_size);
    let normalized_end = addr
        .checked_add(length)
        .and_then(|end| checked_align_up(end, page_size as usize))
        .ok_or(AxError::NoMemory)?;
    let mut length = normalized_end - normalized_start;
    let requested_protection: MappingFlags = permission_flags.into();
    let file_mmap_protection = {
        let mut protection = FileMmapProtection::empty();
        if permission_flags.contains(MmapProt::READ) {
            protection |= FileMmapProtection::READ;
        }
        if permission_flags.contains(MmapProt::WRITE) {
            protection |= FileMmapProtection::WRITE;
        }
        if permission_flags.contains(MmapProt::EXEC) {
            protection |= FileMmapProtection::EXECUTE;
        }
        protection
    };
    let file_mmap_sharing = if map_type == MmapFlags::PRIVATE {
        FileMmapSharing::Private
    } else {
        FileMmapSharing::Shared
    };
    let file_mmap_request = (!is_anonymous_mapping)
        .then(|| {
            FileMmapRequest::try_new(
                offset as u64,
                length,
                page_size as usize,
                file_mmap_protection,
                file_mmap_sharing,
            )
        })
        .transpose()?;

    let mut pinned_fd = pinned_fd;
    let prepared_file_like_plan = match (pinned_fd.as_ref(), file_mmap_request) {
        (Some(handle), Some(request)) => handle.prepare_mmap(request)?,
        (None, None) => None,
        _ => return Err(AxError::BadState),
    };
    let prepared_fixed_mapping = match prepared_file_like_plan {
        Some(plan) => Some(PreparedFixedSharedMapping::try_new(
            pinned_fd.take().ok_or(AxError::BadState)?,
            plan,
        )?),
        None => None,
    };

    let curr = current();
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    let authorized_image = proc_data.thread_image_access_snapshot(thread)?;
    let actor = authorized_image.credential();
    let has_ipc_lock = actor.has_effective_capability(CAP_IPC_LOCK);
    let aspace_handle = authorized_image.aspace().clone();

    if is_anonymous_mapping && permission_flags.contains(MmapProt::WRITE) {
        check_memory_overcommit(length)?;
    }

    // Keep type errors at the historical backend-construction point, but
    // classify the exact OFD pinned above instead of looking up `fd` again.
    let file = pinned_fd
        .map(|handle| {
            handle.downcast::<File>().map_err(|_| {
                if handle.as_ref().downcast_ref::<Directory>().is_some() {
                    AxError::IsADirectory
                } else {
                    AxError::BrokenPipe
                }
            })
        })
        .transpose()?;
    let filesystem_owner_user_ns = file
        .as_ref()
        .map(|_| initial_user_namespace(actor.user_ns()));
    let prepared_file_backend = file
        .as_ref()
        .map(|file| {
            prepare_file_mmap_backend(
                file,
                map_type,
                permission_flags,
                offset,
                page_size,
                &mut length,
            )
        })
        .transpose()?;
    if length == 0 {
        return Err(AxError::InvalidInput);
    }
    let effective_protection = requested_protection;
    mmap_file(
        actor,
        file.as_ref().map(|file| {
            (
                filesystem_owner_user_ns
                    .as_ref()
                    .expect("file mappings freeze a filesystem owner"),
                file,
            )
        }),
        requested_protection,
        effective_protection,
        flags,
    )?;

    // All file/object-specific validation, VFS access, backing allocation, and
    // deferred-owner allocation has completed. From this point through VMA
    // publication, the fixed shared path only moves prepared resources.
    let mut aspace = aspace_handle.lock();
    let start = if map_flags.intersects(MmapFlags::FIXED | MmapFlags::FIXED_NOREPLACE) {
        let dst_addr = VirtAddr::from(normalized_start);
        if !aspace.contains_range(dst_addr, length) {
            return Err(AxError::NoMemory);
        }
        dst_addr
    } else {
        let align = page_size as usize;
        aspace
            .find_kernel_area(
                VirtAddr::from(normalized_start),
                length,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                align,
            )
            .ok_or(AxError::NoMemory)?
    };
    mmap_addr(
        actor,
        authorized_image.owner_user_ns(),
        &aspace_handle,
        start,
    )?;

    let file_mapping = file.map(|file| {
        let sharing = if map_type == MmapFlags::PRIVATE {
            FileMappingSharing::Private
        } else {
            FileMappingSharing::Shared
        };
        let may_protect = match sharing {
            FileMappingSharing::Shared => may_protect_from_file_flags(file.inner().flags()),
            FileMappingSharing::Private => {
                MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE
            }
        };
        FileMappingLease::new(
            file,
            filesystem_owner_user_ns.expect("file mappings freeze a filesystem owner"),
            start,
            offset as u64,
            effective_protection,
            may_protect,
            sharing,
        )
    });
    let backend = if let Some(prepared) = prepared_fixed_mapping {
        prepared.into_backend(start)
    } else {
        match prepared_file_backend {
            Some(PreparedFileMmapBackend::SharedFile {
                cache,
                flags,
                file_end,
            }) => {
                // TODO(mivik): file mmap page size
                Backend::new_file(start, cache, flags, offset, Some(file_end), &aspace_handle)?
            }
            Some(PreparedFileMmapBackend::SharedAnonymous { may_protect }) => {
                Backend::new_shared_with_may_protect(
                    start,
                    Arc::new(SharedPages::new(length, PageSize::Size4K)?),
                    may_protect,
                )
            }
            Some(PreparedFileMmapBackend::Cow {
                location,
                file_end,
                sigbus_on_eof,
            }) => Backend::new_cow(
                start,
                page_size,
                location,
                offset as u64,
                file_end,
                sigbus_on_eof,
            ),
            Some(PreparedFileMmapBackend::AnonymousCow) => Backend::new_alloc(start, page_size),
            Some(PreparedFileMmapBackend::Linear {
                physical_start,
                max_size,
            }) => Backend::new_linear(start, physical_start, max_size),
            None if matches!(map_type, MmapFlags::SHARED | MmapFlags::SHARED_VALIDATE) => {
                Backend::new_shared(start, Arc::new(SharedPages::new(length, page_size)?))
            }
            None if map_type == MmapFlags::PRIVATE => Backend::new_alloc(start, page_size),
            None => return Err(AxError::InvalidInput),
        }
    };
    let growdown_private_anon = map_flags.contains(MmapFlags::GROWDOWN)
        && map_type == MmapFlags::PRIVATE
        && is_anonymous_mapping;
    let backend = match file_mapping {
        Some(file_mapping) => backend.with_file_mapping(file_mapping),
        None => backend,
    };
    let shared_writable_location = (effective_protection.contains(MappingFlags::WRITE)
        && backend
            .file_mapping()
            .is_some_and(|mapping| mapping.sharing() == FileMappingSharing::Shared))
    .then(|| backend.shared_file_location().cloned())
    .flatten();

    let locked_mapping = map_flags.contains(MmapFlags::LOCKED) || aspace.locks_future_mappings();
    if locked_mapping {
        check_mmap_memlock_limit(proc_data, has_ipc_lock, &aspace, start, length)?;
    }

    let populate = map_flags.contains(MmapFlags::POPULATE)
        || map_flags.contains(MmapFlags::LOCKED)
        || (aspace.locks_future_mappings()
            && !aspace.locks_future_mappings_on_fault()
            && !permission_flags.is_empty());
    let mapping_admission = if shared_writable_location.is_some() {
        backend.begin_shared_writable_mapping_admission()?
    } else {
        None
    };
    let privilege_guard = shared_writable_location
        .as_ref()
        .map(begin_shared_writable_mapping_privilege_cleanup)
        .transpose()?;
    if map_flags.contains(MmapFlags::FIXED) && !map_flags.contains(MmapFlags::FIXED_NOREPLACE) {
        aspace.unmap(start, length)?;
        proc_data.clear_mempolicy_range(start.as_usize(), length);
    }
    aspace.map(start, length, effective_protection, populate, backend)?;
    if let Some(admission) = mapping_admission {
        admission
            .complete()
            .expect("writable mapping admission vanished after mmap commit");
    }
    drop(privilege_guard);
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

pub fn sys_mprotect(addr: usize, length: usize, prot: usize) -> AxResult<isize> {
    // TODO: implement PROT_GROWSUP & PROT_GROWSDOWN
    let Some(permission_flags) = MmapProt::from_bits(prot) else {
        return Err(AxError::InvalidInput);
    };
    debug!("sys_mprotect <= addr: {addr:#x}, length: {length:x}, prot: {permission_flags:?}");

    if permission_flags.contains(MmapProt::GROWDOWN | MmapProt::GROWSUP) {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let thread = curr.as_thread();
    let authorized_image = thread.proc_data.thread_image_access_snapshot(thread)?;
    let aspace_handle = authorized_image.aspace().clone();
    let mut aspace = aspace_handle.lock();
    let length = checked_align_up_4k(length).ok_or(AxError::NoMemory)?;
    let start_addr = VirtAddr::from(addr);
    let requested_protection: MappingFlags = permission_flags.into();
    let effective_protection = requested_protection;
    let plan = aspace.prepare_protect(start_addr, length, effective_protection)?;
    authorize_then_commit(
        plan,
        |plan| {
            authorize_all(plan.segments(), |segment| {
                file_mprotect(
                    authorized_image.credential(),
                    authorized_image.owner_user_ns(),
                    segment,
                    requested_protection,
                    effective_protection,
                )
            })
        },
        |plan| commit_shared_writable_protection(plan, effective_protection),
    )?;

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
    if fixed && !may_move {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    let has_ipc_lock = thread.has_effective_capability(CAP_IPC_LOCK);
    remap_user_mapping(
        proc_data,
        has_ipc_lock,
        VirtAddr::from(addr),
        checked_align_up_4k(old_size).ok_or(AxError::InvalidInput)?,
        checked_align_up_4k(new_size).ok_or(AxError::InvalidInput)?,
        may_move,
        fixed,
        VirtAddr::from(new_addr),
    )
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
    use alloc::sync::Arc;
    use core::cell::Cell;

    use axfs_ng_vfs::{Mountpoint, NodePermission, NodeType};
    use thekernel_linux_fd::{DescriptorFlags, FdNumber, FdTable, FdTableId};

    use super::*;
    use crate::{
        file::{FileDescription, FileHandle, FileLike},
        pseudofs::tmp::MemoryFs,
        task::UserNamespace,
    };

    fn mmap_description(name: &str) -> Arc<FileDescription> {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let location = mount
            .root_location()
            .create(
                name,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        FileDescription::new(Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(location),
            FileFlags::READ,
        ))))
        .unwrap()
    }

    #[test]
    fn append_does_not_reduce_mprotect_write_capability() {
        let open_flags = FileFlags::READ | FileFlags::WRITE | FileFlags::APPEND;
        assert!(may_protect_from_file_flags(open_flags).contains(MappingFlags::WRITE));
    }

    #[test]
    fn shared_writable_cleanup_deduplicates_one_inode_across_mount_views() {
        let fs = MemoryFs::new().unwrap();
        let first_mount = Mountpoint::new_root(&fs);
        let second_mount = Mountpoint::new_root(&fs);
        let first = first_mount
            .root_location()
            .create(
                "shared-writable-dedup",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o6755),
            )
            .unwrap();
        let second = second_mount
            .root_location()
            .lookup_no_follow("shared-writable-dedup")
            .unwrap();

        assert!(!first.ptr_eq(&second));
        assert_eq!(executable::key(&first), executable::key(&second));
        let mut locations = Vec::new();
        locations.try_reserve(2).unwrap();
        assert!(push_unique_content_location(&mut locations, &first));
        assert!(!push_unique_content_location(&mut locations, &second));
        assert_eq!(locations.len(), 1);
        assert!(locations[0].1.ptr_eq(&first));
    }

    #[test]
    fn mmap_hooks_run_file_then_address_and_stop_on_denial() {
        let trace = Cell::new(0_u32);
        let effects = Cell::new(0_u32);
        authorize_mmap_candidate(
            || {
                trace.set(trace.get() * 10 + 1);
                Ok(())
            },
            || {
                trace.set(trace.get() * 10 + 2);
                Ok(())
            },
            || {
                effects.set(effects.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(trace.get(), 12);
        assert_eq!(effects.get(), 1);

        trace.set(0);
        effects.set(0);
        assert_eq!(
            authorize_mmap_candidate(
                || {
                    trace.set(trace.get() * 10 + 1);
                    Err(AxError::PermissionDenied)
                },
                || {
                    trace.set(trace.get() * 10 + 2);
                    Ok(())
                },
                || {
                    effects.set(effects.get() + 1);
                    Ok(())
                },
            ),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(trace.get(), 1);
        assert_eq!(effects.get(), 0);

        trace.set(0);
        effects.set(0);
        assert_eq!(
            authorize_mmap_candidate(
                || {
                    trace.set(trace.get() * 10 + 1);
                    Ok(())
                },
                || {
                    trace.set(trace.get() * 10 + 2);
                    Err(AxError::PermissionDenied)
                },
                || {
                    effects.set(effects.get() + 1);
                    Ok(())
                },
            ),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(trace.get(), 12);
        assert_eq!(effects.get(), 0);
    }

    #[test]
    fn mprotect_authorizes_every_segment_before_one_commit() {
        let trace = Cell::new(0_u32);
        let commits = Cell::new(0_u32);
        authorize_then_commit(
            (),
            |_| {
                authorize_all([1, 2, 3], |segment| {
                    trace.set(trace.get() * 10 + segment);
                    Ok(())
                })
            },
            |_| {
                commits.set(commits.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(trace.get(), 123);
        assert_eq!(commits.get(), 1);

        trace.set(0);
        commits.set(0);
        assert_eq!(
            authorize_then_commit(
                (),
                |_| {
                    authorize_all([1, 2, 3], |segment| {
                        trace.set(trace.get() * 10 + segment);
                        if segment == 2 {
                            return Err(AxError::PermissionDenied);
                        }
                        Ok(())
                    })
                },
                |_| {
                    commits.set(commits.get() + 1);
                    Ok(())
                },
            ),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(trace.get(), 12);
        assert_eq!(commits.get(), 0);
    }

    #[test]
    fn mmap_lookup_once_survives_fd_close_and_number_reuse() {
        let first_description = mmap_description("first-mmap-pin");
        let first_id = first_description.id().get();
        let second_description = mmap_description("second-mmap-pin");
        let second_id = second_description.id().get();
        assert_ne!(first_id, second_id);

        let mut table =
            FdTable::<Arc<FileDescription>, 8>::try_new(FdTableId::new(1).unwrap()).unwrap();
        let reservation = table.reserve(3, 4, DescriptorFlags::EMPTY).unwrap();
        let fd = reservation.fd().get() as i32;
        assert!(table.publish(reservation, first_description).is_ok());

        let lookups = Cell::new(0);
        let pinned = lookup_mmap_fd_once(fd, |fd| {
            lookups.set(lookups.get() + 1);
            table
                .get(FdNumber::new(fd as u32))
                .map(|entry| {
                    FileHandle::<dyn FileLike>::from_description_for_test(
                        entry.description().clone(),
                    )
                })
                .map_err(|_| AxError::BadFileDescriptor)
        })
        .unwrap();
        assert_eq!(lookups.get(), 1);

        drop(table.close(FdNumber::new(fd as u32)).unwrap());
        let replacement = table.reserve(3, 4, DescriptorFlags::EMPTY).unwrap();
        assert_eq!(replacement.fd(), FdNumber::new(fd as u32));
        assert!(table.publish(replacement, second_description).is_ok());

        let lease = FileMappingLease::new(
            pinned.downcast::<File>().unwrap(),
            UserNamespace::try_new_root().unwrap(),
            VirtAddr::from(0x4000),
            0,
            MappingFlags::USER | MappingFlags::READ,
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
            FileMappingSharing::Private,
        );
        drop(pinned);
        assert_eq!(lease.ofd_key(), first_id);
        assert_eq!(lease.file().open_file_description_key(), first_id);
        assert_eq!(
            table
                .get(FdNumber::new(fd as u32))
                .unwrap()
                .description()
                .id()
                .get(),
            second_id
        );
        assert_eq!(lookups.get(), 1);
    }

    #[test]
    fn madvise_does_not_treat_a_file_leased_anonymous_cow_as_anonymous() {
        let description = mmap_description("device-anonymous-lease");
        let handle = FileHandle::<dyn FileLike>::from_description_for_test(description)
            .downcast::<File>()
            .unwrap();
        let namespace = UserNamespace::try_new_root().unwrap();
        let start = VirtAddr::from(0x4000);
        let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
        let lease = FileMappingLease::new(
            handle,
            namespace,
            start,
            0,
            flags,
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
            FileMappingSharing::Private,
        );
        let backend = Backend::new_alloc(start, PageSize::Size4K).with_file_mapping(lease);
        let info = classify_madvise_backend(&backend);
        assert!(!info.all_private_anonymous);
        assert!(!info.has_shared_mapping);
    }
}
