use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{CachedFile, FileBackend, FileFlags};
use axfs_ng_vfs::Location;
use axhal::paging::{MappingFlags, PageSize, PreparedPageTableFrames};
use axtask::current;
use linux_raw_sys::general::*;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr, VirtAddrRange};

use crate::{
    file::{
        Directory, File, FileHandle, FileMmapProtection, FileMmapRequest, FileMmapSharing,
        executable, get_file_like,
        privilege_metadata::{
            ContentWritePrivilegeGuard, begin_shared_writable_mapping_privilege_cleanup,
        },
    },
    mm::{
        AddrSpace, Backend, DeferredUffdWake, FileMappingLease, FileMappingSharing,
        PreparedFixedSharedMapping, PreparedProtect, SharedPages, WritableMappingAdmission,
        check_memory_overcommit, checked_align_up, checked_align_up_4k, remap_user_mapping,
    },
    pseudofs::{Device, DeviceMmap},
    task::{
        AsThread, ProcessData,
        security::{file_mprotect, initial_user_namespace, mmap_addr, mmap_file},
    },
};

const READ_IMPLIES_EXEC: u32 = 0x0040_0000;
/// Use Linux's bottom-up compatibility mmap placement rather than the normal
/// append-biased layout.
const ADDR_COMPAT_LAYOUT: u32 = 0x0020_0000;

fn personality_mmap_protection(personality: u32, mut protection: MappingFlags) -> MappingFlags {
    if personality & READ_IMPLIES_EXEC != 0 && protection.contains(MappingFlags::READ) {
        protection |= MappingFlags::EXECUTE;
    }
    protection
}

/// Select an address for a non-fixed mmap.  Linux's compatibility layout is
/// deliberately bottom-up: it must not inherit the normal layout's
/// high-water/append bias just because the process happened to map a stack or
/// loader segment first.
fn find_nonfixed_mmap_area(
    aspace: &AddrSpace,
    personality: u32,
    hint: VirtAddr,
    length: usize,
    limit: VirtAddrRange,
    align: usize,
) -> Option<VirtAddr> {
    if personality & ADDR_COMPAT_LAYOUT != 0 {
        let first = if hint > limit.start {
            hint
        } else {
            limit.start
        };
        aspace
            .find_free_area(first, length, limit, align)
            // Preserve ordinary mmap hint behavior when that hinted scan is
            // exhausted, while retaining bottom-up placement for the retry.
            .or_else(|| {
                (first > limit.start)
                    .then(|| aspace.find_free_area(limit.start, length, limit, align))
                    .flatten()
            })
    } else {
        aspace.find_kernel_area(hint, length, limit, align)
    }
}

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
        crate::mm::check_not_active(location)?;
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
) -> AxResult<DeferredUffdWake> {
    let (admissions, guards) = begin_shared_writable_protection(&plan, effective_protection)?;
    let wake = plan.commit()?;
    for admission in admissions {
        admission
            .complete()
            .expect("writable mapping admission vanished after mprotect commit");
    }
    drop(guards);
    Ok(wake)
}

fn commit_shared_writable_pkey_protection(
    plan: PreparedProtect<'_>,
    effective_protection: MappingFlags,
    demotion: &mut crate::mm::PreparedPkeyDemotion,
    key: axhal::paging::Pkey,
) -> AxResult<DeferredUffdWake> {
    let (admissions, guards) = begin_shared_writable_protection(&plan, effective_protection)?;
    let wake = plan.commit_with_pkey_demotion(demotion, key)?;
    for admission in admissions {
        admission
            .complete()
            .expect("writable mapping admission vanished after pkey commit");
    }
    drop(guards);
    Ok(wake)
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
        /// Preserve semaphore atomicity (mprotect only; no PTE permission effect).
        const SEM = PROT_SEM as usize;
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

fn preflight_mprotect_geometry(
    addr: usize,
    length: usize,
    prot: usize,
) -> AxResult<Option<(usize, VirtAddr)>> {
    let grow_flags = PROT_GROWSDOWN as usize | PROT_GROWSUP as usize;
    if prot & grow_flags == grow_flags {
        return Err(AxError::InvalidInput);
    }
    if !addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(AxError::InvalidInput);
    }
    if length == 0 {
        return Ok(None);
    }
    let length = checked_align_up_4k(length).ok_or(AxError::NoMemory)?;
    let start = VirtAddr::from(addr);
    let end = start.checked_add(length).ok_or(AxError::NoMemory)?;
    if end <= start {
        return Err(AxError::NoMemory);
    }
    Ok(Some((length, end)))
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
    if map_type != MmapFlags::PRIVATE && permission_flags.contains(MmapProt::WRITE) {
        if let Some(file) = file.as_ref() {
            crate::mm::check_not_active(file.inner().location())?;
        }
    }
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
    // READ_IMPLIES_EXEC is suppressed for noexec mounts: Linux leaves such a
    // mapping readable rather than turning the compatibility bit into an
    // executable-map permission failure.
    let mmap_noexec = file
        .as_ref()
        .map(|file| crate::mounts::is_noexec(file.inner().location()))
        .transpose()?
        .unwrap_or(false);
    let read_implies_exec = thread.personality() & READ_IMPLIES_EXEC != 0 && !mmap_noexec;
    let effective_protection = personality_mmap_protection(
        u32::from(read_implies_exec) * READ_IMPLIES_EXEC,
        requested_protection,
    );
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

    if map_flags.contains(MmapFlags::FIXED) && !map_flags.contains(MmapFlags::FIXED_NOREPLACE) {
        // A replacement can cut through a compound shared folio.  Demote the
        // complete alias set before taking this mm's single-map unmap path.
        ensure_4k_granularity_across_aliases(
            &aspace_handle,
            VirtAddr::from(normalized_start),
            length,
        )?;
    }

    // All file/object-specific validation, VFS access, backing allocation, and
    // deferred-owner allocation has completed. From this point through VMA
    // publication, the fixed shared path only moves prepared resources.
    let mut aspace = aspace_handle.lock();
    let mut deferred_uffd_wake = DeferredUffdWake::empty();
    let outcome = (|| {
        let start = if map_flags.intersects(MmapFlags::FIXED | MmapFlags::FIXED_NOREPLACE) {
            let dst_addr = VirtAddr::from(normalized_start);
            if !aspace.contains_range(dst_addr, length) {
                return Err(AxError::NoMemory);
            }
            dst_addr
        } else {
            let align = page_size as usize;
            find_nonfixed_mmap_area(
                &aspace,
                thread.personality(),
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
            let mut may_protect = match sharing {
                FileMappingSharing::Shared => may_protect_from_file_flags(file.inner().flags()),
                FileMappingSharing::Private => {
                    MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE
                }
            };
            if mmap_noexec {
                may_protect.remove(MappingFlags::EXECUTE);
            }
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
                        Arc::new(SharedPages::new_shmem(length, PageSize::Size4K)?),
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
                    Backend::new_shared(start, Arc::new(SharedPages::new_shmem(length, page_size)?))
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

        let locked_mapping =
            map_flags.contains(MmapFlags::LOCKED) || aspace.locks_future_mappings();
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
            .map(|location| {
                crate::mm::check_not_active(location)?;
                begin_shared_writable_mapping_privilege_cleanup(location)
            })
            .transpose()?;
        if map_flags.contains(MmapFlags::FIXED) && !map_flags.contains(MmapFlags::FIXED_NOREPLACE) {
            deferred_uffd_wake.merge(aspace.unmap(start, length)?);
            proc_data.clear_mempolicy_range(start.as_usize(), length);
        }
        let pending_alias = backend
            .shared_backing_key()
            .map(|key| aspace.prepare_shared_alias_binding(key, &aspace_handle))
            .transpose()?
            .flatten();
        aspace.map(start, length, effective_protection, populate, backend)?;
        if let Some(pending_alias) = pending_alias {
            aspace.commit_shared_alias_binding(pending_alias);
        }
        if let Err(error) = aspace.sync_shared_alias_bindings(&aspace_handle) {
            // The reverse-map lease is part of publishing a shared mapping:
            // never expose a new shmem VMA that a later cross-mm collapse
            // cannot discover.  This VMA was just installed by this syscall,
            // so rollback cannot retire an unrelated mapping.
            deferred_uffd_wake.merge(aspace.unmap(start, length)?);
            return Err(error);
        }
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

        Ok(start.as_usize() as isize)
    })();
    drop(aspace);
    deferred_uffd_wake.finish();
    outcome
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
    let length = checked_align_up_4k(length).ok_or(AxError::InvalidInput)?;
    let start_addr = VirtAddr::from(addr);
    ensure_4k_granularity_across_aliases(&aspace_handle, start_addr, length)?;
    let mut aspace = aspace_handle.lock();
    let wake = aspace.unmap(start_addr, length)?;
    proc_data.clear_mempolicy_range(start_addr.as_usize(), length);
    drop(aspace);
    wake.finish();
    Ok(0)
}

/// Linux's deprecated nonlinear shared mapping ABI.  This intentionally does
/// not delegate to `mmap`: the file is selected from the existing VMA (not an
/// fd), and the AddrSpace transaction replaces that VMA at the same address.
pub fn sys_remap_file_pages(
    start: usize,
    size: usize,
    prot: usize,
    pgoff: usize,
    flags: usize,
) -> AxResult<isize> {
    if prot != 0 {
        return Err(AxError::InvalidInput);
    }
    let start = start & !(PageSize::Size4K as usize - 1);
    let size = size & !(PageSize::Size4K as usize - 1);
    if size == 0 || start.checked_add(size).is_none() || pgoff.checked_add(size / PageSize::Size4K as usize).is_none() {
        return Err(AxError::InvalidInput);
    }
    let curr = current();
    let thread = curr.as_thread();
    let image = thread.proc_data.thread_image_access_snapshot(thread)?;
    let aspace_handle = image.aspace().clone();
    let start_addr = VirtAddr::from(start);

    // Read-side snapshot: retain the exact vm_file-like lease across the LSM
    // call, then compare it again while publishing under the write side.
    let (snapshot_flags, lease) = {
        let aspace = aspace_handle.lock();
        aspace.remap_shared_span_snapshot(start_addr, size)?
    };
    let flags = flags & MAP_NONBLOCK as usize;
    // The LSM observes the source VMA's MAP_LOCKED state.  This address
    // space records mlock state at page granularity instead of splitting the
    // VMA, so use the first source page (the VMA selected by Linux) rather
    // than requiring the complete remapped interval to be locked.
    let source_starts_locked = {
        let aspace = aspace_handle.lock();
        aspace.locked_bytes_in_range(start_addr, PageSize::Size4K as usize) != 0
    };
    let raw_flags = (MAP_SHARED | MAP_FIXED | MAP_POPULATE) as usize
        | flags
        | if source_starts_locked { MAP_LOCKED as usize } else { 0 };
    mmap_file(
        image.credential(),
        Some((lease.filesystem_owner_user_ns(), lease.file())),
        snapshot_flags,
        snapshot_flags,
        raw_flags,
    )?;
    mmap_addr(
        image.credential(),
        image.owner_user_ns(),
        &aspace_handle,
        start_addr,
    )?;

    let mut aspace = aspace_handle.lock();
    let (current_flags, current_lease) = aspace.remap_shared_span_snapshot(start_addr, size)?;
    if current_flags != snapshot_flags || current_lease.ofd_key() != lease.ofd_key() {
        return Err(AxError::InvalidInput);
    }
    // The source lock state is part of the LSM's synthetic mmap request.  It
    // is kept in a page-granular ledger here, so revalidate it under the same
    // write-side lock that publishes the alias; otherwise a concurrent mlock
    // could add MAP_LOCKED after LSM authorization.
    if (aspace.locked_bytes_in_range(start_addr, PageSize::Size4K as usize) != 0)
        != source_starts_locked
    {
        return Err(AxError::InvalidInput);
    }
    let populate = flags & MAP_NONBLOCK as usize == 0;
    let outcome = aspace.replace_shared_mapping_span_at_offset(
        &aspace_handle,
        start_addr,
        size,
        pgoff,
        populate,
    );
    drop(aspace);
    outcome.finish()?;
    if populate {
        // Linux treats post-commit population faults as best-effort here: the
        // fixed alias remains installed even when a later page cannot be
        // brought resident.
        let _ = aspace_handle
            .lock()
            .populate_area(start_addr, size, snapshot_flags);
    }
    Ok(0)
}

pub fn sys_mprotect(addr: usize, length: usize, prot: usize) -> AxResult<isize> {
    sys_mprotect_inner(addr, length, prot, None)
}

/// Performs the mprotect VMA walk while holding the address-space lock.  A
/// pkey request is carried in the same replacement flags, so its VMA metadata
/// cannot race a fault between ordinary permission and key publication.
fn sys_mprotect_inner(
    addr: usize,
    length: usize,
    prot: usize,
    requested_pkey: Option<u8>,
) -> AxResult<isize> {
    // TODO: implement PROT_GROWSUP & PROT_GROWSDOWN
    let Some((length, end_addr)) = preflight_mprotect_geometry(addr, length, prot)? else {
        return Ok(0);
    };
    let Some(permission_flags) = MmapProt::from_bits(prot) else {
        return Err(AxError::InvalidInput);
    };
    debug!("sys_mprotect <= addr: {addr:#x}, length: {length:x}, prot: {permission_flags:?}");

    if permission_flags.intersects(MmapProt::GROWDOWN | MmapProt::GROWSUP) {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let thread = curr.as_thread();
    let authorized_image = thread.proc_data.thread_image_access_snapshot(thread)?;
    let aspace_handle = authorized_image.aspace().clone();
    let start_addr = VirtAddr::from(addr);
    ensure_4k_granularity_across_aliases(&aspace_handle, start_addr, length)?;
    let mut aspace = aspace_handle.lock();
    let requested_protection: MappingFlags = permission_flags.into();
    let pkey_leaves = requested_pkey
        .map(|_| aspace.preflight_set_pkey(start_addr, length))
        .transpose()?;

    // Match Linux's per-VMA order: a successful prefix remains protected if a
    // later VMA is unmapped, disallows the requested protection, or is sealed.
    // Re-find each VMA after committing because commit may split or merge it.
    let mut cursor = start_addr;
    let mut wake = DeferredUffdWake::empty();
    let outcome = (|| {
        while cursor < end_addr {
            let Some(area) = aspace.find_area(cursor) else {
                return Err(AxError::NoMemory);
            };
            if area.start() > cursor {
                return Err(AxError::NoMemory);
            }
            let segment_end = area.end().min(end_addr);
            let segment_size = segment_end.sub_addr(cursor);
            let may_execute = area
                .backend()
                .file_mapping()
                .is_none_or(|mapping| mapping.may_protect().contains(MappingFlags::EXECUTE));
            let effective_protection = if may_execute {
                personality_mmap_protection(thread.personality(), requested_protection)
            } else {
                requested_protection
            }
            // mprotect changes ordinary permissions but must retain the VMA's
            // protection-key attribute so a later demand fault or COW leaf
            // is coloured exactly like the resident mapping.
            .with_pkey(requested_pkey.unwrap_or_else(|| area.flags().pkey()));
            let plan = aspace.prepare_protect(cursor, segment_size, effective_protection)?;
            let segment_wake = authorize_then_commit(
                plan,
                |plan| {
                    for segment in plan.segments() {
                        // LSM authorization deliberately precedes the mseal check,
                        // as it does in Linux's mprotect VMA walk.
                        file_mprotect(
                            authorized_image.credential(),
                            authorized_image.owner_user_ns(),
                            segment,
                            requested_protection,
                            effective_protection,
                        )?;
                        if segment.backend().is_sealed() {
                            return Err(AxError::OperationNotPermitted);
                        }
                    }
                    Ok(())
                },
                |plan| commit_shared_writable_protection(plan, effective_protection),
            )?;
            wake.merge(segment_wake);
            cursor = segment_end;
        }
        Ok(0)
    })();
    if outcome.is_ok() {
        if let (Some(key), Some(leaves)) = (requested_pkey, pkey_leaves) {
            let pkey = axhal::paging::Pkey::new(key).expect("validated pkey");
            let mut pt = aspace.page_table_mut().cursor();
            for (vaddr, ..) in leaves {
                pt.set_pkey(vaddr, pkey)
                    .expect("preflighted pkey leaf must remain mapped");
            }
            drop(pt);
            aspace.synchronize_pte_mutation();
        }
    }
    drop(aspace);
    wake.finish();
    outcome
}

/// Linux x86 `pkey_mprotect(2)`.  Key validation happens before the ordinary
/// mprotect transaction so an invalid/free key never changes VMA permissions.
/// Key zero remains valid without allocation; all nonzero keys must be owned
/// by this mm at the instant the page-table transaction starts.
pub fn sys_pkey_mprotect(addr: usize, length: usize, prot: usize, pkey: i32) -> AxResult<isize> {
    // Keep mprotect's ABI validation/range ordering, including a zero-length
    // request's no-op behavior, before inspecting the key allocation map.
    let Some((length, _)) = preflight_mprotect_geometry(addr, length, prot)? else {
        return Ok(0);
    };
    let curr = current();
    let thread = curr.as_thread();
    if !axhal::asm::pkeys_enabled() {
        return Err(AxError::StorageFull);
    }
    if !(0..16).contains(&pkey) || (pkey != 0 && !thread.proc_data.pkey_is_allocated(pkey)) {
        return Err(AxError::InvalidInput);
    }

    let Some(permission_flags) = MmapProt::from_bits(prot) else {
        return Err(AxError::InvalidInput);
    };
    if permission_flags.intersects(MmapProt::GROWDOWN | MmapProt::GROWSUP) {
        return Err(AxError::InvalidInput);
    }

    // Unlike ordinary mprotect's Linux prefix semantics, pkey_mprotect must
    // never publish a new VMA key without changing every corresponding leaf.
    // Preflight all fallible VMA/PTE resources and authorize the complete
    // range before the one prepared commit.
    let authorized_image = thread.proc_data.thread_image_access_snapshot(thread)?;
    let aspace_handle = authorized_image.aspace().clone();
    let mut aspace = aspace_handle.lock();
    let start = VirtAddr::from(addr);
    let leaves = aspace.preflight_set_pkey(start, length)?;
    let mut demotion = aspace.prepare_pkey_demotion(start, length)?;
    let requested: MappingFlags = permission_flags.into();
    let plan = aspace.prepare_pkey_protect(
        start,
        length,
        requested,
        pkey as u8,
        thread.personality() & READ_IMPLIES_EXEC != 0,
    )?;
    for segment in plan.segments() {
        file_mprotect(
            authorized_image.credential(),
            authorized_image.owner_user_ns(),
            segment,
            requested,
            segment.new_flags(),
        )?;
        if segment.backend().is_sealed() {
            return Err(AxError::OperationNotPermitted);
        }
    }
    let key = axhal::paging::Pkey::new(pkey as u8).expect("validated pkey");
    let wake = commit_shared_writable_pkey_protection(plan, requested, &mut demotion, key)?;
    let mut pt = aspace.page_table_mut().cursor();
    for (vaddr, ..) in leaves {
        pt.set_pkey(vaddr, key)
            .expect("preflighted pkey leaf must remain mapped");
    }
    drop(pt);
    // `leaves` contains each original huge leaf once. The prepared demotion
    // has already set all P1 children above; repeating its first child here
    // is harmless and keeps fully covered huge leaves on the same path.
    aspace.synchronize_pte_mutation();
    drop(aspace);
    wake.finish();
    Ok(0)
}

/// Linux x86 `pkey_alloc(2)`.  Only the two PKRU access-disable bits are
/// accepted, and allocation always chooses the lowest free nonzero key.
pub fn sys_pkey_alloc(flags: u32, access_rights: u32) -> AxResult<isize> {
    const PKEY_ACCESS_MASK: u32 = 0x3;
    if flags != 0 || access_rights & !PKEY_ACCESS_MASK != 0 {
        return Err(AxError::InvalidInput);
    }
    if !axhal::asm::pkeys_enabled() {
        return Err(AxError::StorageFull);
    }
    let curr = current();
    let thread = curr.as_thread();
    let key = thread.proc_data.allocate_pkey()?;
    if let Err(error) = thread.set_pkey_access_rights(key, access_rights) {
        // Allocation and PKRU initialization are one syscall transaction.
        thread
            .proc_data
            .free_pkey(key as i32)
            .expect("new pkey must be allocated");
        return Err(error);
    }
    Ok(key as isize)
}

/// Linux x86 `pkey_free(2)`.  Deliberately does not modify PTEs or PKRU; a
/// reallocated key makes any old mappings visible again, just as on Linux.
pub fn sys_pkey_free(pkey: i32) -> AxResult<isize> {
    let curr = current();
    curr.as_thread().proc_data.free_pkey(pkey)?;
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

/// Linux 6.12 `mseal(2)`: `flags` is reserved and the length is rounded up
/// before checking the complete mapped VMA span.
fn normalize_mseal_length(addr: usize, length: usize, flags: usize) -> AxResult<usize> {
    if flags != 0 || !addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(AxError::InvalidInput);
    }
    let length = checked_align_up_4k(length).ok_or(AxError::InvalidInput)?;
    addr.checked_add(length).ok_or(AxError::InvalidInput)?;
    Ok(length)
}

pub fn sys_mseal(addr: usize, length: usize, flags: usize) -> AxResult<isize> {
    let length = normalize_mseal_length(addr, length, flags)?;
    if length == 0 {
        return Ok(0);
    }

    let curr = current();
    let aspace_handle = curr.as_thread().proc_data.aspace();
    aspace_handle.lock().seal(VirtAddr::from(addr), length)?;
    Ok(0)
}

fn madvise_discard_behavior(advice: u32) -> bool {
    matches!(
        advice,
        MADV_FREE
            | MADV_DONTNEED
            | MADV_DONTNEED_LOCKED
            | MADV_REMOVE
            | MADV_DONTFORK
            | MADV_WIPEONFORK
    )
}

/// Applies the subset of `madvise` which can be safely driven against a
/// caller-pinned foreign address space.  This function intentionally has no
/// current-task lookup: `process_madvise` must retain the pidfd image it
/// authorized rather than switching the current address-space context.
pub(crate) fn process_madvise_willneed(
    aspace: &mut AddrSpace,
    addr: usize,
    length: usize,
) -> AxResult {
    if !addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(AxError::InvalidInput);
    }
    if length == 0 {
        return Ok(());
    }
    let (start, length) = validate_page_aligned_range(addr, length)?;
    let end = start + length;
    let mut cursor = start;

    // Linux applies advice to the mapped prefix and reports ENOMEM only on
    // reaching a hole.  Do not preflight the complete range: that would
    // incorrectly discard useful file-cache work before a later hole.
    while cursor < end {
        let Some(area) = aspace.find_area(cursor) else {
            return Err(AxError::NoMemory);
        };
        if area.start() > cursor {
            return Err(AxError::NoMemory);
        }
        let area_end = area.end().min(end);
        let backend = area.backend().clone();
        backend.prefetch_file_backed(VirtAddrRange::new(cursor, area_end), aspace)?;
        cursor = area_end;
    }
    Ok(())
}

/// Demotes resident file-cache pages in each mapped segment.  No fault is
/// taken and anonymous mappings retain their data unchanged.
pub(crate) fn process_madvise_cold(aspace: &mut AddrSpace, addr: usize, length: usize) -> AxResult {
    process_madvise_walk(aspace, addr, length, true, |aspace, backend, range| {
        // Clearing the x86 accessed state is a real LRU demotion for every
        // resident mapping kind. File-backed mappings additionally demote
        // their cache entry; anonymous and shmem backing is retained because
        // this kernel has no swap representation.
        aspace.cold_resident_pages(range)?;
        if backend.has_file_cache_backing() {
            backend.cold_file_pages(range)?;
        }
        Ok(())
    })
}

/// Collects file-cache eviction work while the target VMA layout is stable.
/// The caller must run the returned work only after dropping the address-space
/// lock: cache eviction listeners need to detach aliases from that same mm.
pub(crate) fn process_madvise_collect_pageout(
    aspace: &mut AddrSpace,
    addr: usize,
    length: usize,
    work: &mut Vec<(Backend, VirtAddrRange)>,
) -> AxResult {
    process_madvise_walk(aspace, addr, length, true, |aspace, backend, range| {
        // PAGEOUT first demotes resident PTEs. Without swap this is the
        // Linux outcome for private anonymous and shmem leaves: retain data
        // and make it reclaim-eligible, rather than falsely discarding it or
        // rejecting an otherwise valid advisory request.
        aspace.cold_resident_pages(range)?;
        // File-private COW mappings retain their source cache separately from
        // their anonymous leaves and therefore need the same PAGEOUT work as
        // shared file mappings. Anonymous/shmem leaves have no swap target,
        // so their completed PTE demotion is the real no-swap result.
        if backend.has_file_cache_backing() {
            work.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            work.push((backend, range));
        }
        Ok(())
    })
}

fn process_madvise_walk(
    aspace: &mut AddrSpace,
    addr: usize,
    length: usize,
    reject_locked: bool,
    mut apply: impl FnMut(&mut AddrSpace, Backend, VirtAddrRange) -> AxResult,
) -> AxResult {
    if !addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(AxError::InvalidInput);
    }
    if length == 0 {
        return Ok(());
    }
    let (start, length) = validate_page_aligned_range(addr, length)?;
    let end = start + length;
    let mut cursor = start;
    while cursor < end {
        let Some(area) = aspace.find_area(cursor) else {
            return Err(AxError::NoMemory);
        };
        if area.start() > cursor {
            return Err(AxError::NoMemory);
        }
        let area_end = area.end().min(end);
        if reject_locked {
            let locked = aspace.locked_segments_in_range(cursor, area_end.sub_addr(cursor));
            if let Some((locked_start, _)) = locked.first().copied() {
                if cursor < locked_start {
                    apply(
                        aspace,
                        area.backend().clone(),
                        VirtAddrRange::new(cursor, locked_start),
                    )?;
                }
                return Err(AxError::InvalidInput);
            }
        }
        let backend = area.backend().clone();
        apply(aspace, backend, VirtAddrRange::new(cursor, area_end))?;
        cursor = area_end;
    }
    Ok(())
}

/// Restores 4 KiB geometry for a range which may intersect a shared compound
/// folio.  A normal alias-preserving PMD can be expanded under one mm lock,
/// but a `SharedPages` folio owns replacement frames for every alias and must
/// be demoted as one cross-mm transaction.
pub(crate) fn ensure_4k_granularity_across_aliases(
    aspace_handle: &Arc<axsync::Mutex<AddrSpace>>,
    start: VirtAddr,
    length: usize,
) -> AxResult {
    if length == 0 {
        return Ok(());
    }
    let end = start.checked_add(length).ok_or(AxError::InvalidInput)?;
    let mut cursor = VirtAddr::from(start.as_usize() & !(PageSize::Size2M as usize - 1));
    while cursor < end {
        let compound = {
            let aspace = aspace_handle.lock();
            match (
                aspace.shared_pages_at(cursor),
                aspace.shared_backing_offset_at(cursor),
            ) {
                (Some(pages), Some(offset)) => {
                    let start_index = offset / PAGE_SIZE_4K;
                    (pages.page_size() == PageSize::Size4K
                        && offset.is_multiple_of(PageSize::Size2M as usize)
                        && pages.has_4k_folio(start_index)
                        && aspace
                            .page_table()
                            .query(cursor)
                            .is_ok_and(|(_, _, size)| size == PageSize::Size2M))
                    .then_some((pages, start_index))
                }
                _ => None,
            }
        };
        if let Some((pages, start_index)) = compound {
            demote_shared_folio_across_aliases(aspace_handle, pages, start_index)?;
        } else {
            aspace_handle
                .lock()
                .ensure_4k_granularity(cursor, PageSize::Size2M as usize)?;
        }
        cursor = cursor
            .checked_add(PageSize::Size2M as usize)
            .ok_or(AxError::InvalidInput)?;
    }
    Ok(())
}

fn demote_shared_folio_across_aliases(
    target: &Arc<axsync::Mutex<AddrSpace>>,
    pages: Arc<SharedPages>,
    start_index: usize,
) -> AxResult {
    let (_mutation, aliases) = crate::mm::reserve_alias_mutation(pages.backing_key());
    let mut participants = aliases
        .into_iter()
        .filter_map(|alias| alias.revalidate().map(|aspace| (alias.address_space_id(), aspace)))
        .collect::<Vec<_>>();
    let target_id = target.lock().address_space_id();
    if !participants.iter().any(|(_, aspace)| Arc::ptr_eq(aspace, target)) {
        participants.push((target_id, target.clone()));
    }
    participants.sort_unstable_by_key(|(id, _)| *id);
    participants.dedup_by(|(_, left), (_, right)| Arc::ptr_eq(left, right));

    let mut guards = Vec::new();
    guards
        .try_reserve_exact(participants.len())
        .map_err(|_| AxError::NoMemory)?;
    for (_, participant) in &participants {
        guards.push(participant.lock());
    }

    // Snapshot every fallible backing resource before touching any PTE.  The
    // old 4 KiB frames remain folio-owned until the final commit.
    let frames = pages.demote_4k_folio_frames(start_index)?;
    let mut plans = Vec::new();
    for (guard_index, guard) in guards.iter().enumerate() {
        for alias_start in guard.shared_folio_alias_starts(&pages, start_index)? {
            let flags = guard.preflight_shared_folio_demotion_2m(alias_start, &pages, start_index)?;
            plans.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            plans.push((guard_index, alias_start, flags));
        }
    }
    if plans.is_empty() {
        return Err(AxError::BadState);
    }
    let mut tables = Vec::new();
    tables
        .try_reserve_exact(plans.len())
        .map_err(|_| AxError::NoMemory)?;
    for _ in &plans {
        tables.push(PreparedPageTableFrames::try_new(1).map_err(|_| AxError::NoMemory)?);
    }

    let mut protected = 0usize;
    for &(guard_index, alias_start, flags) in &plans {
        if let Err(error) = guards[guard_index]
            .write_protect_shared_folio_demotion_2m(alias_start, flags)
        {
            for &(rollback_guard, rollback_start, rollback_flags) in plans[..protected].iter().rev() {
                guards[rollback_guard]
                    .restore_shared_folio_demotion_pmd_permissions(rollback_start, rollback_flags)?;
            }
            return Err(error);
        }
        protected += 1;
    }

    let mut published = Vec::new();
    published
        .try_reserve_exact(plans.len())
        .map_err(|_| AxError::NoMemory)?;
    for (index, &(guard_index, alias_start, flags)) in plans.iter().enumerate() {
        match guards[guard_index].publish_shared_folio_demotion_2m(
            alias_start,
            &frames,
            flags,
            &mut tables[index],
        ) {
            Ok(replacement) => published.push((guard_index, replacement)),
            Err(error) => {
                for (rollback_guard, replacement) in published.drain(..).rev() {
                    guards[rollback_guard].rollback_shared_folio_demotion_2m(replacement)?;
                }
                for &(rollback_guard, rollback_start, rollback_flags) in plans.iter().rev() {
                    guards[rollback_guard]
                        .restore_shared_folio_demotion_pmd_permissions(rollback_start, rollback_flags)?;
                }
                return Err(error);
            }
        }
    }

    // No stale PMD may retain writable access while the folio is copied back.
    for guard in guards.iter_mut() {
        drop(guard.synchronize_tlb_after_mutation());
    }
    if let Err(error) = pages.demote_4k_folio(start_index) {
        for (rollback_guard, replacement) in published.drain(..).rev() {
            guards[rollback_guard].rollback_shared_folio_demotion_2m(replacement)?;
        }
        return Err(error);
    }
    for (guard_index, replacement) in published {
        guards[guard_index]
            .restore_shared_folio_permissions_2m(replacement.start, replacement.flags)?;
    }
    Ok(())
}

/// Returns the PMD-aligned subrange fully contained in a page-rounded
/// MADV_COLLAPSE request.  `None` is a valid no-op when no full PMD fits.
fn collapse_full_pmd_range(addr: usize, length: usize) -> AxResult<Option<(usize, usize)>> {
    // Unlike most madvise range operations, Linux rejects an unaligned
    // original MADV_COLLAPSE address.  Length is still rounded up below.
    if !addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(AxError::InvalidInput);
    }
    let unit = PageSize::Size2M as usize;
    let (page_start, page_length) = validate_page_aligned_range(addr, length)?;
    if page_length == 0 {
        return Ok(None);
    }
    let page_end = page_start + page_length;
    let start = page_start
        .as_usize()
        .checked_add(unit - 1)
        .map(|value| value & !(unit - 1))
        .ok_or(AxError::InvalidInput)?;
    let end = page_end.as_usize() & !(unit - 1);
    Ok((start < end).then_some((start, end)))
}

pub(crate) fn process_madvise_collapse(
    aspace_handle: &Arc<axsync::Mutex<AddrSpace>>,
    addr: usize,
    length: usize,
) -> AxResult {
    let shared_key = {
        let aspace = aspace_handle.lock();
        let Some((mut cursor, end)) = collapse_full_pmd_range(addr, length)? else {
            return Ok(());
        };
        // Select from the first *eligible PMD*, not the raw user address:
        // MADV_COLLAPSE deliberately permits a leading partial VMA.
        let key = aspace.shared_backing_key_at(VirtAddr::from(cursor));
        while cursor < end {
            let candidate = aspace.shared_backing_key_at(VirtAddr::from(cursor));
            if candidate != key {
                // A transaction cannot safely change from a private/file PMD
                // to a shared backing (or between backings) mid-request.
                return Err(AxError::InvalidInput);
            }
            cursor = cursor
                .checked_add(PageSize::Size2M as usize)
                .ok_or(AxError::InvalidInput)?;
        }
        key
    };
    if let Some(key) = shared_key {
        // Alias leases are weak and may disappear between snapshot and lock.
        // Upgrade and de-duplicate first, then acquire every live mm in the
        // sole global order (AddressSpaceId).  No VMA mutation happens before
        // all participants are locked.
        let (_mutation, aliases) = crate::mm::reserve_alias_mutation(key);
        let mut participants = aliases
            .into_iter()
            .filter_map(|alias| alias.revalidate().map(|aspace| (alias.address_space_id(), aspace)))
            .collect::<Vec<_>>();
        let target_id = aspace_handle.lock().address_space_id();
        if !participants.iter().any(|(_, alias)| Arc::ptr_eq(alias, aspace_handle)) {
            participants.push((target_id, aspace_handle.clone()));
        }
        participants.sort_unstable_by_key(|(id, _)| *id);
        participants.dedup_by(|(_, left), (_, right)| Arc::ptr_eq(left, right));
        let target_index = participants
            .iter()
            .position(|(_, alias)| Arc::ptr_eq(alias, aspace_handle))
            .ok_or(AxError::BadState)?;
        let mut guards = Vec::new();
        guards
            .try_reserve_exact(participants.len())
            .map_err(|_| AxError::NoMemory)?;
        for (_, participant) in &participants {
            guards.push(participant.lock());
        }
        return process_madvise_collapse_shared_locked(&mut guards, target_index, addr, length);
    }
    let mut aspace = aspace_handle.lock();
    process_madvise_collapse_locked(&mut aspace, addr, length)
}

fn process_madvise_collapse_shared_locked(
    guards: &mut [axsync::MutexGuard<'_, AddrSpace>],
    target_index: usize,
    addr: usize,
    length: usize,
) -> AxResult {
    let Some((mut cursor, end)) = collapse_full_pmd_range(addr, length)? else {
        return Ok(());
    };
    while cursor < end {
        let start = VirtAddr::from(cursor);
        let Some(pages) = guards[target_index].shared_pages_at(start) else {
            let result = if guards[target_index]
                .find_area(start)
                .is_some_and(|area| area.backend().is_private_cow())
            {
                guards[target_index].collapse_private_cow_2m(start)
            } else {
                guards[target_index].collapse_alias_preserving_2m(start)
            };
            result?;
            cursor = cursor.checked_add(PageSize::Size2M as usize).ok_or(AxError::InvalidInput)?;
            continue;
        };
        let backing_offset = guards[target_index]
            .shared_backing_offset_at(start)
            .ok_or(AxError::BadState)?;
        if !backing_offset.is_multiple_of(PageSize::Size2M as usize) {
            return Err(AxError::InvalidInput);
        }
        let start_index = backing_offset / PAGE_SIZE_4K;
        // Fixed shared objects can expose raw kernel atomic handles. Those
        // handles are intentionally lifetime-pinned to their base frames and
        // cannot be redirected by a userspace PTE transaction.
        if pages.is_fixed() {
            return Err(AxError::InvalidInput);
        }
        let mut plans = Vec::new();
        for (guard_index, guard) in guards.iter().enumerate() {
            for alias_start in guard.shared_folio_alias_starts(&pages, start_index)? {
                let flags = guard.preflight_shared_folio_collapse_2m(alias_start, &pages, start_index)?;
                plans.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                plans.push((guard_index, alias_start, flags));
            }
        }
        if !plans.iter().any(|(guard_index, alias_start, _)| {
            *guard_index == target_index && *alias_start == start
        }) {
            return Err(AxError::BadState);
        }
        let mut published = Vec::new();
        published
            .try_reserve_exact(plans.len())
            .map_err(|_| AxError::NoMemory)?;
        for &(guard_index, alias_start, flags) in &plans {
            if let Err(error) = guards[guard_index]
                .write_protect_shared_folio_collapse_2m(alias_start, flags)
            {
                for &(restore_guard, restore_start, restore_flags) in &plans {
                    if restore_guard == guard_index && restore_start == alias_start {
                        break;
                    }
                    guards[restore_guard]
                        .restore_shared_folio_permissions_2m(restore_start, restore_flags)?;
                }
                return Err(error);
            }
        }
        let folio = match pages.promote_4k_folio(start_index) {
            Ok(folio) => folio,
            Err(error) => {
                for &(guard_index, alias_start, flags) in &plans {
                    guards[guard_index].restore_shared_folio_permissions_2m(alias_start, flags)?;
                }
                return Err(error);
            }
        };
        for &(guard_index, alias_start, flags) in &plans {
            match guards[guard_index].publish_shared_folio_collapse_2m(alias_start, folio, flags) {
                Ok(replacement) => published.push((guard_index, replacement)),
                Err(error) => {
                    for (rollback_guard, replacement) in published.drain(..).rev() {
                        guards[rollback_guard].rollback_shared_folio_collapse_2m(replacement)?;
                    }
                    pages.demote_4k_folio(start_index)?;
                    for &(restore_guard, restore_start, restore_flags) in &plans {
                        guards[restore_guard]
                            .restore_shared_folio_permissions_2m(restore_start, restore_flags)?;
                    }
                    return Err(error);
                }
            }
        }
        for (commit_guard, replacement) in published {
            guards[commit_guard].commit_shared_folio_collapse_2m(replacement);
        }
        cursor = cursor.checked_add(PageSize::Size2M as usize).ok_or(AxError::InvalidInput)?;
    }
    Ok(())
}

fn process_madvise_collapse_locked(
    aspace: &mut AddrSpace,
    addr: usize,
    length: usize,
) -> AxResult {
    // MADV_COLLAPSE takes ordinary page-granular user ranges. Only PMD units
    // wholly enclosed by the page-rounded range are candidates; a partial
    // leading or trailing PMD retains its 4 KiB representation. This is
    // deliberately not a nonzero/multiple-of-2MiB argument gate.
    let Some((mut cursor, end)) = collapse_full_pmd_range(addr, length)? else {
        return Ok(());
    };
    let unit = PageSize::Size2M as usize;
    let mut first_error = None;
    while cursor < end {
        let start = VirtAddr::from(cursor);
        let result = if aspace
            .find_area(start)
            .is_some_and(|area| area.backend().is_private_cow())
        {
            aspace.collapse_private_cow_2m(start)
        } else {
            aspace.collapse_alias_preserving_2m(start)
        };
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
        cursor = cursor.checked_add(unit).ok_or(AxError::InvalidInput)?;
    }
    first_error.map_or(Ok(()), Err)
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

    if madvise_discard_behavior(advice) && aspace.sealed_ro_anon_in_range(start, length) {
        return Err(AxError::OperationNotPermitted);
    }

    match advice {
        // Hints the kernel may safely ignore once the range is known-valid.
        MADV_NORMAL | MADV_RANDOM | MADV_SEQUENTIAL | MADV_WILLNEED => {
            inspect_madvise_range(&aspace, start, length)?;
            Ok(0)
        }
        MADV_DONTFORK | MADV_DOFORK => {
            inspect_madvise_range(&aspace, start, length)?;
            // Fork policy ranges can split a PMD.  Establish 4 KiB geometry
            // first, rather than leaving later fork/duplicate transactions
            // with a policy boundary through one huge COW leaf.
            aspace.ensure_4k_granularity(start, length)?;
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
            aspace.ensure_4k_granularity(start, length)?;
            aspace.set_wipe_on_fork(start, length, advice == MADV_WIPEONFORK)?;
            Ok(0)
        }
        MADV_KEEPONFORK => {
            inspect_madvise_range(&aspace, start, length)?;
            aspace.ensure_4k_granularity(start, length)?;
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
    use memory_addr::PAGE_SIZE_4K;
    use thekernel_linux_fd::{DescriptorFlags, FdNumber, FdTable, FdTableId};

    use super::*;
    use crate::{
        file::{FileDescription, FileHandle, FileLike},
        mm::Backend,
        pseudofs::tmp::MemoryFs,
        task::UserNamespace,
    };

    #[test]
    fn compat_layout_uses_bottom_up_mmap_placement() {
        let base = VirtAddr::from(0x1000);
        let mut aspace = AddrSpace::new_empty(base, 0x10_000).unwrap();
        // Leave a low hole below this existing VMA so append-biased and
        // compatibility placement have different observable results.
        aspace
            .map(
                VirtAddr::from(0x5000),
                PAGE_SIZE_4K,
                MappingFlags::USER,
                false,
                Backend::new_alloc(VirtAddr::from(0x5000), PageSize::Size4K),
            )
            .unwrap();
        let limit = VirtAddrRange::new(aspace.base(), aspace.end());

        assert_eq!(
            find_nonfixed_mmap_area(
                &aspace,
                ADDR_COMPAT_LAYOUT,
                base,
                PAGE_SIZE_4K,
                limit,
                PAGE_SIZE_4K
            ),
            Some(base),
        );
        assert_eq!(
            find_nonfixed_mmap_area(&aspace, 0, base, PAGE_SIZE_4K, limit, PAGE_SIZE_4K),
            Some(VirtAddr::from(0x6000)),
        );
    }

    #[test]
    fn mseal_normalizes_length_and_rejects_invalid_geometry() {
        assert_eq!(normalize_mseal_length(0x4000, 1, 0), Ok(PAGE_SIZE_4K));
        assert_eq!(normalize_mseal_length(0x4000, 0, 0), Ok(0));
        assert_eq!(
            normalize_mseal_length(0x4001, 0, 0),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            normalize_mseal_length(0x4000, 1, 1),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            normalize_mseal_length(usize::MAX & !(PAGE_SIZE_4K - 1), 1, 0),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn mseal_madvise_discard_set_matches_linux_612() {
        assert!(madvise_discard_behavior(MADV_FREE));
        assert!(madvise_discard_behavior(MADV_DONTNEED));
        assert!(madvise_discard_behavior(MADV_DONTNEED_LOCKED));
        assert!(madvise_discard_behavior(MADV_REMOVE));
        assert!(madvise_discard_behavior(MADV_DONTFORK));
        assert!(madvise_discard_behavior(MADV_WIPEONFORK));
        assert!(!madvise_discard_behavior(MADV_COLD));
        assert!(!madvise_discard_behavior(MADV_PAGEOUT));
    }

    #[test]
    fn collapse_uses_page_rounded_range_and_skips_partial_pmds() {
        let unit = PageSize::Size2M as usize;

        assert_eq!(
            collapse_full_pmd_range(0x1234, 1),
            Err(AxError::InvalidInput)
        );
        assert_eq!(collapse_full_pmd_range(unit, 0), Ok(None));
        assert_eq!(
            collapse_full_pmd_range(0x1000, 2 * unit),
            Ok(Some((unit, 2 * unit)))
        );
        assert_eq!(collapse_full_pmd_range(unit + 1, unit - 1), Ok(None));
        assert_eq!(
            collapse_full_pmd_range(unit, unit),
            Ok(Some((unit, 2 * unit)))
        );
        assert_eq!(
            collapse_full_pmd_range(unit, unit - 1),
            Ok(Some((unit, 2 * unit)))
        );
    }

    #[test]
    fn mprotect_zero_length_preflight_matches_linux_order() {
        assert_eq!(
            preflight_mprotect_geometry(0x4001, 0, 0),
            Err(AxError::InvalidInput)
        );
        assert_eq!(preflight_mprotect_geometry(0x4000, 0, usize::MAX), Ok(None));
    }

    #[test]
    fn mprotect_prot_sem_is_accepted_without_page_permission_effect() {
        let protection = MmapProt::from_bits(PROT_SEM as usize).unwrap();
        assert_eq!(MappingFlags::from(protection), MappingFlags::USER);
        assert!(preflight_mprotect_geometry(0x4000, 1, PROT_SEM as usize).is_ok());
    }

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
    fn mprotect_commits_each_authorized_vma_before_later_failure() {
        let trace = Cell::new(0_u32);
        let commits = Cell::new(0_u32);
        for segment in [1, 2, 3] {
            authorize_then_commit(
                segment,
                |segment| {
                    trace.set(trace.get() * 10 + *segment);
                    Ok(())
                },
                |_| {
                    commits.set(commits.get() + 1);
                    Ok(())
                },
            )
            .unwrap();
        }
        assert_eq!(trace.get(), 123);
        assert_eq!(commits.get(), 3);

        trace.set(0);
        commits.set(0);
        let result: AxResult<()> = (|| {
            for segment in [1, 2, 3] {
                authorize_then_commit(
                    segment,
                    |segment| {
                        trace.set(trace.get() * 10 + *segment);
                        if *segment == 2 {
                            return Err(AxError::PermissionDenied);
                        }
                        Ok(())
                    },
                    |_| {
                        commits.set(commits.get() + 1);
                        Ok(())
                    },
                )?;
            }
            Ok(())
        })();
        assert_eq!(result, Err(AxError::PermissionDenied));
        assert_eq!(trace.get(), 12);
        assert_eq!(commits.get(), 1);
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

    #[test]
    fn read_implies_exec_promotes_only_readable_protections() {
        let read = MappingFlags::USER | MappingFlags::READ;
        assert!(
            personality_mmap_protection(READ_IMPLIES_EXEC, read).contains(MappingFlags::EXECUTE)
        );
        assert!(
            !personality_mmap_protection(READ_IMPLIES_EXEC, MappingFlags::USER)
                .contains(MappingFlags::EXECUTE)
        );
        assert!(!personality_mmap_protection(0, read).contains(MappingFlags::EXECUTE));
    }
}
