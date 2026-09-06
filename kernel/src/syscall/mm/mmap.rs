use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{CachedFile, FileBackend, FileFlags};
use axfs_ng_vfs::Location;
use axhal::paging::{MappingFlags, PageSize, PreparedPageTableFrames};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::*;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use thekernel_linux_arch_x86_64::{ArchPolicyError, PKEY_RIGHTS_MASK, PkeyPlan};

use crate::{
    file::{
        Directory, File, FileHandle, FileMmapProtection, FileMmapRequest, FileMmapSharing,
        executable, get_file_like, inode_flags,
        permission::{check_inode_permissions_with_security_and_idmap, current_vfs_security},
        privilege_metadata::{
            ContentWritePrivilegeGuard, begin_shared_writable_mapping_privilege_cleanup,
        },
    },
    mm::{
        AddrSpace, Backend, DeferredUffdWake, FileMappingLease, FileMappingSharing,
        MadviseReadahead, MadviseThp, PreparedFixedSharedMapping, PreparedProtect,
        SharedFolioDemotionReplacement, SharedFolioPteRedirect,
        SharedFolioPteReplacement, SharedPages, WritableMappingAdmission, check_memory_overcommit,
        check_rlimit_as_growth, checked_align_up, checked_align_up_4k, remap_user_mapping,
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
const SHADOW_STACK_SET_TOKEN: usize = 1;
const SHADOW_STACK_MIN_ADDR: usize = 1usize << 32;
/// One explicit population can cross several file VMAs.  Each retry rechecks
/// the complete operation after a lock-external eviction transaction; bound
/// the loop so pinned/full caches retain ordinary ENOMEM behavior rather than
/// spinning a syscall forever.
const EXPLICIT_POPULATE_RETRY_LIMIT: usize = 64;

enum MadviseRemoveTarget {
    File {
        file: FileHandle<File>,
        offset: u64,
        length: u64,
    },
    AnonymousShared {
        pages: Arc<SharedPages>,
        offset: usize,
        length: usize,
    },
}

/// Captures exact backing ownership while the VMA topology is stable.  The
/// retained file handles and shared-page Arcs survive fd close, VMA splits and
/// the lock drop required before filesystem mutation.
fn collect_madvise_remove_targets(
    aspace: &AddrSpace,
    start: VirtAddr,
    length: usize,
) -> AxResult<Vec<MadviseRemoveTarget>> {
    let end = start.checked_add(length).ok_or(AxError::InvalidInput)?;
    let mut cursor = start;
    let mut targets = Vec::new();

    while cursor < end {
        let area = aspace.find_area(cursor).ok_or(AxError::NoMemory)?;
        if area.start() > cursor {
            return Err(AxError::NoMemory);
        }
        let segment_end = area.end().min(end);
        let segment_length = segment_end.sub_addr(cursor);
        match area.backend() {
            Backend::File(_) => {
                let lease = area.backend().file_mapping().ok_or(AxError::InvalidInput)?;
                if lease.sharing() != FileMappingSharing::Shared {
                    return Err(AxError::InvalidInput);
                }
                let offset = lease.file_offset_at(cursor).ok_or(AxError::InvalidInput)?;
                let length = u64::try_from(segment_length).map_err(|_| AxError::InvalidInput)?;
                targets.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                targets.push(MadviseRemoveTarget::File {
                    file: lease.file().clone(),
                    offset,
                    length,
                });
            }
            Backend::Shared(shared)
                if shared.pages().supports_madv_remove()
                    && area.backend().file_mapping().is_none() =>
            {
                let offset = shared
                    .backing_offset(cursor.as_usize())
                    .ok_or(AxError::InvalidInput)?;
                targets.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                targets.push(MadviseRemoveTarget::AnonymousShared {
                    pages: shared.pages().clone(),
                    offset,
                    length: segment_length,
                });
            }
            Backend::Linear(_) | Backend::Cow(_) | Backend::Shared(_) => {
                return Err(AxError::InvalidInput);
            }
        }
        cursor = segment_end;
    }
    Ok(targets)
}

/// Populates an already-installed VMA range without allowing a file-cache
/// replacement transaction to run under the address-space mutex.
///
/// `revalidate` owns the operation-specific VMA/permission checks and runs on
/// every attempt, including after a successful reclaim.  This is shared by
/// MAP_POPULATE, MADV_POPULATE and remap_file_pages' post-commit best-effort
/// fill so `ResourceBusy` stays an internal cache-pressure token.
fn populate_explicit_with_reclaim(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    start: VirtAddr,
    size: usize,
    access_flags: MappingFlags,
    revalidate: impl Fn(&AddrSpace) -> AxResult<()>,
) -> AxResult<()> {
    for _ in 0..EXPLICIT_POPULATE_RETRY_LIMIT {
        let caches = {
            let mut aspace = aspace_handle.lock();
            revalidate(&aspace)?;
            match aspace.populate_area(start, size, access_flags) {
                Ok(()) => return Ok(()),
                Err(error) if error.canonicalize() == AxError::ResourceBusy => {
                    aspace.file_caches_for_population_retry(start, size)?
                }
                Err(error) => return Err(error),
            }
        };
        let mut reclaimed = false;
        for cache in caches {
            reclaimed |= cache.reclaim_one()?;
        }
        if !reclaimed {
            return Err(AxError::NoMemory);
        }
    }
    Err(AxError::NoMemory)
}

fn personality_mmap_protection(personality: u32, mut protection: MappingFlags) -> MappingFlags {
    if personality & READ_IMPLIES_EXEC != 0 && protection.contains(MappingFlags::READ) {
        protection |= MappingFlags::EXECUTE;
    }
    protection
}

/// Linux MDWE refuses a transition that gains execute permission from a
/// writable (or potentially writable) VMA.  The check deliberately uses the
/// VMA's maximum protection as well as its current bits: dropping WRITE first
/// must not create an execute-gain escape hatch.
fn mdwe_refuses_execute_gain(
    proc_data: &ProcessData,
    old_flags: MappingFlags,
    new_flags: MappingFlags,
    may_protect: MappingFlags,
) -> bool {
    const PR_MDWE_REFUSE_EXEC_GAIN: u8 = 1;
    proc_data.mdwe() & PR_MDWE_REFUSE_EXEC_GAIN != 0
        && new_flags.contains(MappingFlags::EXECUTE)
        && !old_flags.contains(MappingFlags::EXECUTE)
        && (old_flags.contains(MappingFlags::WRITE) || may_protect.contains(MappingFlags::WRITE))
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
            .find_free_area_avoiding_shadow_stack_guards(first, length, limit, align)
            // Preserve ordinary mmap hint behavior when that hinted scan is
            // exhausted, while retaining bottom-up placement for the retry.
            .or_else(|| {
                (first > limit.start)
                    .then(|| {
                        aspace.find_free_area_avoiding_shadow_stack_guards(
                            limit.start,
                            length,
                            limit,
                            align,
                        )
                    })
                    .flatten()
            })
    } else {
        aspace.find_kernel_area(hint, length, limit, align)
    }
}

fn lookup_mmap_fd_once<T>(fd: i32, lookup: impl FnOnce(i32) -> AxResult<T>) -> AxResult<T> {
    lookup(fd).map_err(|_| AxError::BadFileDescriptor)
}

/// Linux x86-64 `map_shadow_stack(2)`.  Explicit mappings are intentionally
/// not registered as default task stacks: the ABI owns their lifetime.
pub fn sys_map_shadow_stack(addr: usize, size: usize, flags: usize) -> AxResult<isize> {
    if !axhal::asm::user_shadow_stack_enabled() {
        return Err(AxError::OperationNotSupported);
    }
    if flags & !SHADOW_STACK_SET_TOKEN != 0 || size == 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & SHADOW_STACK_SET_TOKEN != 0 && size < core::mem::size_of::<u64>() {
        return Err(LinuxError::ENOSPC.into());
    }
    if flags & SHADOW_STACK_SET_TOKEN != 0 && !size.is_multiple_of(core::mem::size_of::<u64>()) {
        return Err(AxError::InvalidInput);
    }
    let requested_size = size;
    let size = checked_align_up_4k(size).ok_or(LinuxError::EOVERFLOW)?;
    if addr != 0 && addr < SHADOW_STACK_MIN_ADDR {
        return Err(LinuxError::ERANGE.into());
    }
    if addr != 0 && !addr.is_multiple_of(PAGE_SIZE_4K) {
        return Err(AxError::InvalidInput);
    }
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let aspace_handle = proc_data.aspace();
    let mut aspace = aspace_handle.lock();
    let start = if addr == 0 {
        let total = size
            .checked_add(PAGE_SIZE_4K)
            .ok_or(LinuxError::EOVERFLOW)?;
        aspace
            .find_kernel_area(
                VirtAddr::from(SHADOW_STACK_MIN_ADDR),
                total,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                PAGE_SIZE_4K,
            )
            .and_then(|base| base.checked_add(PAGE_SIZE_4K))
            .ok_or(AxError::NoMemory)?
    } else {
        let start = VirtAddr::from(addr);
        if !aspace.contains_range(start, size) {
            return Err(AxError::NoMemory);
        }
        start
    };
    if aspace.mapped_bytes_in_range(start, size)? != 0 {
        return Err(AxError::AlreadyExists);
    }
    check_rlimit_as_growth(proc_data, &aspace, size)?;
    aspace.map(
        start,
        size,
        MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE | MappingFlags::SHADOW_STACK,
        false,
        Backend::new_alloc(start, PageSize::Size4K),
    )?;
    let result = (|| {
        if flags & SHADOW_STACK_SET_TOKEN != 0 {
            // Linux records the token at requested-size offset, not at the end of
            // the page-rounded VMA.
            let token = start.as_usize() + requested_size - core::mem::size_of::<u64>();
            aspace.populate_area(
                VirtAddr::from(token & !(PAGE_SIZE_4K - 1)),
                PAGE_SIZE_4K,
                MappingFlags::READ,
            )?;
            let (frame, leaf, _) = aspace.page_table().query(VirtAddr::from(token))?;
            if !leaf.contains(MappingFlags::SHADOW_STACK) {
                return Err(AxError::BadState);
            }
            let offset = token & (PAGE_SIZE_4K - 1);
            unsafe {
                (axhal::mem::phys_to_virt(frame).as_mut_ptr().add(offset) as *mut u64)
                    .write((token + 8) as u64 | 1)
            };
        }
        Ok(start.as_usize() as isize)
    })();
    if result.is_err()
        && let Ok(wake) = aspace.unmap(start, size)
    {
        wake.finish();
    }
    result
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
        pages: Arc<SharedPages>,
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
    DeviceSharedPages(Arc<SharedPages>),
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
                    pages: Arc::try_new(SharedPages::new_shmem(*length, page_size)?)
                        .map_err(|_| AxError::NoMemory)?,
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
                DeviceMmap::SharedPages(pages) if offset == 0 => {
                    *length = (*length).min(pages.len().saturating_mul(PAGE_SIZE_4K));
                    Ok(PreparedFileMmapBackend::DeviceSharedPages(pages))
                }
                DeviceMmap::SharedPages(_) => Err(AxError::InvalidInput),
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
                Some(DeviceMmap::SharedPages(pages)) if offset == 0 => {
                    *length = (*length).min(pages.len().saturating_mul(PAGE_SIZE_4K));
                    Ok(PreparedFileMmapBackend::DeviceSharedPages(pages))
                }
                Some(DeviceMmap::SharedPages(_)) => Err(AxError::InvalidInput),
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
    MmapProt::from_bits(prot).ok_or(AxError::InvalidInput)?;
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
        /// x86-64 placement constraint for legacy 32-bit pointer consumers.
        const BIT32 = MAP_32BIT as usize;
        /// Suppress MAP_POPULATE I/O; the VMA is still installed normally.
        const NONBLOCK = MAP_NONBLOCK as usize;
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
    // MAP_HUGETLB on a file is not a request to promote an ordinary file
    // mapping.  It is admitted only when that exact FileLike has exported a
    // prevalidated fixed SharedPages plan with the requested huge geometry.
    // `prepare_mmap` is the typed provider/VFS capability boundary: it owns
    // page-size, range, sharing, and protection validation before any VMA
    // lock is acquired.  A normal file therefore retains Linux's EINVAL
    // result instead of silently receiving an anonymous huge backing.
    if map_flags.contains(MmapFlags::HUGE)
        && !is_anonymous_mapping
        && prepared_file_like_plan.is_none()
    {
        return Err(AxError::InvalidInput);
    }
    // A regular-file provider can return a fixed SharedPages plan (hugetlbfs
    // does). Keep a second typed lease for the VFS/LSM and mount-policy
    // checks below before the generic handle moves into the prepared owner.
    // Non-VFS fixed providers remain detached from this optional path.
    let prepared_vfs_file = prepared_file_like_plan
        .is_some()
        .then(|| {
            pinned_fd
                .as_ref()
                .expect("a prepared file-like plan retains its pinned fd")
                .clone()
                .downcast::<File>()
                .ok()
        })
        .flatten();
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
    let has_prepared_fixed_mapping = prepared_fixed_mapping.is_some();
    let file = if has_prepared_fixed_mapping {
        prepared_vfs_file
    } else {
        pinned_fd
            .map(|handle| {
                handle.downcast::<File>().map_err(|_| {
                    if handle.as_ref().downcast_ref::<Directory>().is_some() {
                        AxError::IsADirectory
                    } else {
                        AxError::BrokenPipe
                    }
                })
            })
            .transpose()?
    };
    if map_type != MmapFlags::PRIVATE
        && permission_flags.contains(MmapProt::WRITE)
        && let Some(file) = file.as_ref()
    {
        crate::mm::check_not_active(file.inner().location())?;
    }
    let filesystem_owner_user_ns = file
        .as_ref()
        .map(|_| initial_user_namespace(actor.user_ns()));
    let prepared_file_backend = if has_prepared_fixed_mapping {
        None
    } else {
        file.as_ref()
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
            .transpose()?
    };
    if length == 0 {
        return Err(AxError::InvalidInput);
    }
    // Allocate every anonymous shared backing before alias admission.  A
    // pending registry generation must exist before MAP_FIXED can retire an
    // old VMA, while the wait for a THP mutation remains lock-external.
    let prepared_anonymous_shared_pages = match prepared_file_backend.as_ref() {
        Some(PreparedFileMmapBackend::SharedAnonymous { pages, .. })
        | Some(PreparedFileMmapBackend::DeviceSharedPages(pages)) => Some(pages.clone()),
        None if matches!(map_type, MmapFlags::SHARED | MmapFlags::SHARED_VALIDATE) => Some(
            Arc::try_new(SharedPages::new_shmem(length, page_size)?)
                .map_err(|_| AxError::NoMemory)?,
        ),
        _ => None,
    };
    let pending_alias = prepared_fixed_mapping
        .as_ref()
        .map(PreparedFixedSharedMapping::shared_backing_key)
        .or_else(|| {
            prepared_anonymous_shared_pages
                .as_ref()
                .map(|pages| pages.backing_key())
        })
        .map(|key| crate::mm::prepare_shared_alias_binding_lock_external(key, &aspace_handle))
        .transpose()?;
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
    if proc_data.mdwe() & 1 != 0
        && effective_protection.contains(MappingFlags::WRITE)
        && effective_protection.contains(MappingFlags::EXECUTE)
    {
        return Err(AxError::OperationNotPermitted);
    }
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

    // Capture pathname and inode identity while the file lease is still
    // available. Publication happens only after the VMA transaction below
    // commits, so failed mappings never leak speculative MMAP records.
    let mut perf_mmap_name = Vec::new();
    let (perf_mmap_major, perf_mmap_minor, perf_mmap_ino) = if let Some(file) = file.as_ref() {
        let location = file.inner().location();
        let path = location.absolute_path()?;
        perf_mmap_name
            .try_reserve_exact(path.as_bytes().len())
            .map_err(|_| AxError::NoMemory)?;
        perf_mmap_name.extend_from_slice(path.as_bytes());
        let metadata = location.metadata()?;
        let device = crate::mounts::linux_device_id(metadata.device);
        (device.major(), device.minor(), metadata.inode)
    } else {
        (0, 0, 0)
    };
    let perf_mmap_prot = u32::from(effective_protection.contains(MappingFlags::READ)) * PROT_READ
        | u32::from(effective_protection.contains(MappingFlags::WRITE)) * PROT_WRITE
        | u32::from(effective_protection.contains(MappingFlags::EXECUTE)) * PROT_EXEC;
    let perf_mmap_info = crate::perf_records::MmapInfo {
        filename: &perf_mmap_name,
        major: perf_mmap_major,
        minor: perf_mmap_minor,
        ino: perf_mmap_ino,
        // The VFS has no inode-generation field; zero truthfully denotes an
        // unavailable generation instead of inventing one from ctime.
        ino_generation: 0,
        prot: perf_mmap_prot,
        flags: flags as u32,
        executable: effective_protection.contains(MappingFlags::EXECUTE),
    };

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
    let _uprobe_topology = crate::uprobe::registration_topology_gate();
    let (outcome, deferred_uffd_wake) = {
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
                let limit = if map_flags.contains(MmapFlags::BIT32) {
                    // MAP_32BIT has no effect on MAP_FIXED mappings.  For an
                    // ordinary x86-64 placement Linux restricts the search to
                    // the first 2 GiB of the user address space.
                    VirtAddrRange::new(
                        aspace.base(),
                        VirtAddr::from(aspace.end().as_usize().min(1usize << 31)),
                    )
                } else {
                    VirtAddrRange::new(aspace.base(), aspace.end())
                };
                find_nonfixed_mmap_area(
                    &aspace,
                    thread.personality(),
                    VirtAddr::from(normalized_start),
                    length,
                    limit,
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

            let covered = if map_flags.intersects(MmapFlags::FIXED | MmapFlags::FIXED_NOREPLACE) {
                aspace.mapped_bytes_in_range(start, length)?
            } else {
                0
            };
            if map_flags.contains(MmapFlags::FIXED_NOREPLACE) && covered != 0 {
                return Err(AxError::AlreadyExists);
            }
            let address_space_growth = if map_flags.contains(MmapFlags::FIXED)
                && !map_flags.contains(MmapFlags::FIXED_NOREPLACE)
            {
                length.checked_sub(covered).ok_or(AxError::BadState)?
            } else {
                length
            };
            check_rlimit_as_growth(proc_data, &aspace, address_space_growth)?;

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
                        Backend::new_file(
                            start,
                            cache,
                            flags,
                            offset,
                            Some(file_end),
                            &aspace_handle,
                        )?
                    }
                    Some(PreparedFileMmapBackend::SharedAnonymous { pages, may_protect }) => {
                        Backend::new_shared_with_may_protect(start, pages, may_protect)
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
                    Some(PreparedFileMmapBackend::AnonymousCow) => {
                        Backend::new_alloc(start, page_size)
                    }
                    Some(PreparedFileMmapBackend::Linear {
                        physical_start,
                        max_size,
                    }) => Backend::new_linear(start, physical_start, max_size),
                    Some(PreparedFileMmapBackend::DeviceSharedPages(pages)) => {
                        Backend::new_shared(start, pages)
                    }
                    None if matches!(map_type, MmapFlags::SHARED | MmapFlags::SHARED_VALIDATE) => {
                        Backend::new_shared(
                            start,
                            prepared_anonymous_shared_pages.expect(
                                "shared anonymous backing was prepared before alias admission",
                            ),
                        )
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

            let secret_mapping = backend.is_secret();
            let locked_mapping = secret_mapping
                || map_flags.contains(MmapFlags::LOCKED)
                || aspace.locks_future_mappings();
            if locked_mapping {
                check_mmap_memlock_limit(proc_data, has_ipc_lock, &aspace, start, length)?;
            }

            let populate = (map_flags.contains(MmapFlags::POPULATE)
                && !map_flags.contains(MmapFlags::NONBLOCK))
                || map_flags.contains(MmapFlags::LOCKED)
                || (aspace.locks_future_mappings()
                    && !aspace.locks_future_mappings_on_fault()
                    && !permission_flags.is_empty());
            let fixed_replacement = map_flags.contains(MmapFlags::FIXED)
                && !map_flags.contains(MmapFlags::FIXED_NOREPLACE);
            // `replace_mapping_fixed_with` obtains its own prepared fixed file
            // admission while the exact incoming backend is still detached.  An
            // ordinary WritableMappingAdmission here would activate that same
            // registration a second time and make a failed replacement leak the
            // outer activation.  MAP_FIXED_NOREPLACE remains an ordinary map and
            // therefore keeps the historical admission path.
            let mapping_admission = if !fixed_replacement && shared_writable_location.is_some() {
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
            // The participant snapshots every uprobe/XOL authority while both
            // topology and mm gates are held.  Its rollback runs before the
            // fixed-replacement guard restores old leaves, so a failed MAP_FIXED
            // cannot lose an old breakpoint, XOL mapping, or USDT counter byte.
            let mut fixed_uprobe_transition = fixed_replacement.then(|| {
                crate::uprobe::PreparedFixedUprobeTransition::prepare_or_defer_locked(
                    &aspace_handle,
                    &aspace,
                    start,
                    length,
                    &backend,
                    effective_protection,
                )
            });
            let best_effort_secret_populate = secret_mapping && populate;
            if fixed_replacement {
                let participant = fixed_uprobe_transition
                    .as_mut()
                    .expect("fixed replacement prepared its uprobe participant");
                match aspace.replace_mapping_fixed_with(
                    start,
                    length,
                    effective_protection,
                    backend,
                    locked_mapping,
                    participant,
                ) {
                    Ok(wake) => deferred_uffd_wake.merge(wake),
                    Err(error) => {
                        // The incoming pending alias lease has not committed, but
                        // the primitive intentionally keeps all old leases until
                        // its caller ends the transition.  Prune against the
                        // restored old topology before returning the preserved
                        // error.
                        aspace.finish_shared_alias_binding_transition();
                        return Err(error.into_error());
                    }
                }
                // A fixed replacement is now visible.  Linux clears policy for
                // the replaced interval only after mmap_region has published it;
                // preserving the old policy on a rolled-back participant failure
                // is required for the old mapping to remain indistinguishable.
                proc_data.clear_mempolicy_range(start.as_usize(), length);
            } else if let Err(error) = aspace.map(
                start,
                length,
                effective_protection,
                // File-cache reclaim may need this or another address-space lock.
                // Install the VMA first, then populate through the lock-external
                // retry helper below.
                false,
                backend,
            ) {
                return Err(error);
            } else {
                crate::uprobe::install_mapping_best_effort_locked(
                    &aspace_handle,
                    &mut aspace,
                    start,
                    length,
                );
            }
            if let Some(pending_alias) = pending_alias {
                aspace.commit_shared_alias_binding(pending_alias);
            }
            if fixed_replacement {
                // `replace_mapping_fixed_with` preserves old reverse-map leases
                // through the PTE/VMA transaction.  Finish after committing an
                // incoming lease regardless of whether the new backend itself is
                // shared, otherwise a fixed replacement from shared to private
                // leaves a stale old lease behind.
                aspace.finish_shared_alias_binding_transition();
            }
            // VM_LOCKED and grow-down identity are part of the published VMA,
            // not post-population annotations.  Install them while this exact
            // mapping is still protected by the address-space mutex; otherwise a
            // concurrent unmap/replacement during lock-external population could
            // make us annotate an unrelated successor VMA.
            if secret_mapping || map_flags.contains(MmapFlags::LOCKED) {
                aspace.set_locked(start, length, true)?;
            }
            if growdown_private_anon {
                aspace.mark_growdown(start);
            }
            if populate && !best_effort_secret_populate {
                drop(aspace);
                // Linux publishes mmap first and then invokes mm_populate() after
                // dropping mmap_lock; mm_populate's fault/allocation result is
                // intentionally not returned by mmap(2).  In particular, a
                // MAP_FIXED replacement stays destructive after this point rather
                // than silently claiming transactional population atomicity.
                let _ = populate_explicit_with_reclaim(
                    &aspace_handle,
                    start,
                    length,
                    effective_protection,
                    |aspace| {
                        if !aspace.contains_range(start, length)
                            || !aspace.can_access_range(start, length, effective_protection)
                        {
                            return Err(AxError::BadAddress);
                        }
                        Ok(())
                    },
                );
                aspace = aspace_handle.lock();
            }
            if best_effort_secret_populate {
                // EOF is a future SIGBUS fault, not an mmap failure.
                let _ = aspace.populate_area(start, length, effective_protection);
            }
            if let Some(admission) = mapping_admission {
                admission
                    .complete()
                    .expect("writable mapping admission vanished after mmap commit");
            }
            drop(privilege_guard);

            Ok(start.as_usize() as isize)
        })();
        (outcome, deferred_uffd_wake)
    };
    deferred_uffd_wake.finish();
    match outcome {
        Ok(mapped) => {
            // PERF_RECORD_MMAP/MMAP2 exposes pgoff in pages, not bytes.
            thread.perf_emit_mmap(
                mapped as u64,
                length as u64,
                (offset / PAGE_SIZE_4K) as u64,
                &perf_mmap_info,
            );
            Ok(mapped)
        }
        Err(error) => Err(error),
    }
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
    let _uprobe_topology = crate::uprobe::registration_topology_gate();
    let mut aspace = aspace_handle.lock();
    let wake = aspace.unmap(start_addr, length)?;
    proc_data.clear_mempolicy_range(start_addr.as_usize(), length);
    let reconcile = crate::uprobe::reconcile_mm_locked_gated(&aspace_handle, &mut aspace);
    drop(aspace);
    wake.finish();
    reconcile?;
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
    if size == 0
        || start.checked_add(size).is_none()
        || pgoff
            .checked_add(size / PageSize::Size4K as usize)
            .is_none()
    {
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
        | if source_starts_locked {
            MAP_LOCKED as usize
        } else {
            0
        };
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
    if let Some(retry) = aspace.file_eviction_retry_for_range(start_addr, size) {
        drop(aspace);
        retry.wait()?;
        // The source VMA snapshot, MAP_LOCKED observation and LSM decision
        // must all be revalidated after a cache eviction terminal edge.
        return sys_remap_file_pages(start, size, prot, pgoff, flags);
    }
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
        // Commit the replacement first. Cache eviction may synchronously
        // walk this same mm, so requested population happens lock-external
        // below after the replacement transaction has finished.
        false,
    );
    drop(aspace);
    outcome.finish()?;
    if populate {
        // Linux treats post-commit population faults as best-effort here: the
        // fixed alias remains installed even when a later page cannot be
        // brought resident. ResourceBusy is nevertheless not surfaced or
        // left as a lock-order violation: reclaim and revalidate internally.
        let _ = populate_explicit_with_reclaim(
            &aspace_handle,
            start_addr,
            size,
            snapshot_flags,
            |aspace| {
                let (current_flags, current_lease) =
                    aspace.remap_shared_span_snapshot(start_addr, size)?;
                (current_flags == snapshot_flags && current_lease.ofd_key() == lease.ofd_key())
                    .then_some(())
                    .ok_or(AxError::InvalidInput)
            },
        );
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
    let Some((mut length, end_addr)) = preflight_mprotect_geometry(addr, length, prot)? else {
        return Ok(0);
    };
    let Some(permission_flags) = MmapProt::from_bits(prot) else {
        return Err(AxError::InvalidInput);
    };
    debug!("sys_mprotect <= addr: {addr:#x}, length: {length:x}, prot: {permission_flags:?}");

    if permission_flags.contains(MmapProt::GROWSUP) {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let thread = curr.as_thread();
    let authorized_image = thread.proc_data.thread_image_access_snapshot(thread)?;
    let aspace_handle = authorized_image.aspace().clone();
    let mut start_addr = VirtAddr::from(addr);
    if permission_flags.contains(MmapProt::GROWDOWN) {
        let aspace = aspace_handle.lock();
        start_addr = aspace
            .growdown_start_containing(start_addr)
            .ok_or(AxError::InvalidInput)?;
        // The requested end is retained: PROT_GROWSDOWN extends only the
        // lower bound, never the user-supplied upper bound.
        if end_addr <= start_addr {
            return Err(AxError::InvalidInput);
        }
        length = end_addr.sub_addr(start_addr);
    }
    ensure_4k_granularity_across_aliases(&aspace_handle, start_addr, length)?;
    // This loop begins each permission-changing walk with the address-space
    // mutex held, so a newly published eviction fence cannot race the test.
    // The wait is lock-external and the next iteration revalidates every VMA.
    loop {
        let uprobe_topology = crate::uprobe::registration_topology_gate();
        let mut aspace = aspace_handle.lock();
        if let Some(retry) = aspace.file_eviction_retry_for_range(start_addr, length) {
            drop(aspace);
            drop(uprobe_topology);
            retry.wait()?;
            continue;
        }
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
                let original_protection = area.flags();
                let shadow_stack = area.flags().contains(MappingFlags::SHADOW_STACK);
                // CET leaves are architecturally W=0,D=1; generic mprotect may
                // retain read access but never convert them into ordinary pages.
                // A shadow stack keeps its type across mprotect.  Access can be
                // reduced to PROT_NONE or PROT_READ, but ordinary WRITE/EXECUTE
                // permission is never admitted for a SHSTK VMA.
                if shadow_stack
                    && requested_protection != MappingFlags::READ
                    && requested_protection != (MappingFlags::READ | MappingFlags::WRITE)
                    && !requested_protection.is_empty()
                {
                    return Err(AxError::InvalidInput);
                }
                let may_execute = area
                    .backend()
                    .file_mapping()
                    .is_none_or(|mapping| mapping.may_protect().contains(MappingFlags::EXECUTE));
                let may_protect = area.backend().file_mapping().map_or(
                    MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
                    |mapping| mapping.may_protect(),
                );
                let mut effective_protection = if may_execute {
                    personality_mmap_protection(thread.personality(), requested_protection)
                } else {
                    requested_protection
                }
                // mprotect changes ordinary permissions but must retain the VMA's
                // protection-key attribute so a later demand fault or COW leaf
                // is coloured exactly like the resident mapping.
                .with_pkey(requested_pkey.unwrap_or_else(|| area.flags().pkey()));
                if shadow_stack {
                    effective_protection =
                        (effective_protection - MappingFlags::EXECUTE) | MappingFlags::SHADOW_STACK;
                }
                if mdwe_refuses_execute_gain(
                    &thread.proc_data,
                    area.flags(),
                    effective_protection,
                    may_protect,
                ) {
                    return Err(AxError::OperationNotPermitted);
                }
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
                if effective_protection.contains(MappingFlags::WRITE) {
                    // mprotect has just installed writable PTEs for this
                    // exact committed span. Consume any MADV_FREE generation
                    // now; waiting for a write fault would be too late.
                    aspace.consume_madvise_free_write_range(cursor, segment_size);
                }
                if let Err(error) =
                    crate::uprobe::reconcile_mm_locked_gated(&aspace_handle, &mut aspace)
                {
                    // A permission gain may not become visible without its
                    // registered breakpoint set. Revert only this segment;
                    // earlier Linux-style successful prefixes were already
                    // reconciled before their commit edge advanced.
                    let rollback =
                        aspace.prepare_protect(cursor, segment_size, original_protection)?;
                    wake.merge(rollback.commit()?);
                    // Best effort retires any partial probe installation. If
                    // it still cannot complete, the original non-executable
                    // permission remains the fail-closed execution boundary.
                    let _ = crate::uprobe::reconcile_mm_locked_gated(&aspace_handle, &mut aspace);
                    return Err(error);
                }
                cursor = segment_end;
            }
            Ok(0)
        })();
        if outcome.is_ok()
            && let (Some(key), Some(leaves)) = (requested_pkey, pkey_leaves)
        {
            let pkey = axhal::paging::Pkey::new(key).expect("validated pkey");
            let mut pt = aspace.page_table_mut().cursor();
            for (vaddr, _, _, page_size) in leaves {
                if vaddr < start_addr || vaddr + page_size as usize > end_addr {
                    continue;
                }
                pt.set_pkey(vaddr, pkey)
                    .expect("preflighted pkey leaf must remain mapped");
            }
            drop(pt);
            aspace.synchronize_pte_mutation();
        }
        drop(aspace);
        wake.finish();
        let result = outcome?;
        drop(uprobe_topology);
        return Ok(result);
    }
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
    let start = VirtAddr::from(addr);
    loop {
        let uprobe_topology = crate::uprobe::registration_topology_gate();
        let mut aspace = aspace_handle.lock();
        if let Some(retry) = aspace.file_eviction_retry_for_range(start, length) {
            drop(aspace);
            drop(uprobe_topology);
            retry.wait()?;
            continue;
        }
        aspace.reject_special_mapping_mutation(start, length)?;
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
        let mut exec_gains = Vec::new();
        exec_gains
            .try_reserve(plan.segments().count())
            .map_err(|_| AxError::NoMemory)?;
        for segment in plan.segments() {
            let may_protect = segment.backend().file_mapping().map_or(
                MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
                |mapping| mapping.may_protect(),
            );
            if mdwe_refuses_execute_gain(
                &thread.proc_data,
                segment.flags(),
                segment.new_flags(),
                may_protect,
            ) {
                return Err(AxError::OperationNotPermitted);
            }
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
            if segment.new_flags().contains(MappingFlags::EXECUTE)
                && !segment.flags().contains(MappingFlags::EXECUTE)
            {
                exec_gains.push(segment.affected());
            }
        }
        // The authorization plan borrows the VMA tree. Drop it before the
        // projected-probe transaction, then rebuild the identical protection
        // plan under the same mm/topology/credential gates.
        drop(plan);
        for range in exec_gains {
            if let Err(error) = crate::uprobe::install_projected_exec_mapping_locked(
                &aspace_handle,
                &mut aspace,
                range.start,
                range.end.sub_addr(range.start),
            ) {
                let _ = crate::uprobe::reconcile_mm_locked_gated(&aspace_handle, &mut aspace);
                return Err(error);
            }
        }
        let plan = aspace.prepare_pkey_protect(
            start,
            length,
            requested,
            pkey as u8,
            thread.personality() & READ_IMPLIES_EXEC != 0,
        )?;
        let key = axhal::paging::Pkey::new(pkey as u8).expect("validated pkey");
        let wake = match commit_shared_writable_pkey_protection(plan, requested, &mut demotion, key)
        {
            Ok(wake) => wake,
            Err(error) => {
                let _ = crate::uprobe::reconcile_mm_locked_gated(&aspace_handle, &mut aspace);
                return Err(error);
            }
        };
        if requested.contains(MappingFlags::WRITE) {
            aspace.consume_madvise_free_write_range(start, length);
        }
        let mut pt = aspace.page_table_mut().cursor();
        for &(vaddr, _, _, page_size) in &leaves {
            // Partial leaves were demoted and keyed only within the request.
            if vaddr < start || vaddr + page_size as usize > start + length {
                continue;
            }
            pt.set_pkey(vaddr, key)
                .expect("preflighted pkey leaf must remain mapped");
        }
        drop(pt);
        // `leaves` contains each original huge leaf once. The prepared demotion
        // has already keyed the requested P1 children above; only fully
        // covered original leaves are updated by this final pass.
        aspace.synchronize_pte_mutation();
        if crate::uprobe::reconcile_mm_locked_gated(&aspace_handle, &mut aspace).is_err() {
            // Every newly executable segment already owns its complete probe
            // set. Remaining stale-counter/retirement work stays registry-owned
            // and is retried in task context without changing syscall outcome.
            crate::deferred_work::wake_uprobe_restore_worker();
        }
        drop(aspace);
        wake.finish();
        drop(uprobe_topology);
        return Ok(0);
    }
}

/// Linux x86 `pkey_alloc(2)`.  Only the two PKRU access-disable bits are
/// accepted, and allocation always chooses the lowest free nonzero key.
pub fn sys_pkey_alloc(flags: usize, access_rights: usize) -> AxResult<isize> {
    // Linux takes unsigned long arguments and rejects invalid rights before
    // checking hardware support or allocating from the finite key domain.
    if flags != 0 || access_rights & !(PKEY_RIGHTS_MASK as usize) != 0 {
        return Err(AxError::InvalidInput);
    }
    if !axhal::asm::pkeys_enabled() {
        return Err(AxError::StorageFull);
    }
    let curr = current();
    let thread = curr.as_thread();
    let key = thread.proc_data.allocate_pkey()?;
    let plan = PkeyPlan::new(key, access_rights as u32).map_err(|error| match error {
        ArchPolicyError::InvalidPkeyRights
        | ArchPolicyError::InvalidPkey
        | ArchPolicyError::DefaultPkey => AxError::InvalidInput,
        _ => AxError::InvalidInput,
    });
    if let Err(error) =
        plan.and_then(|plan| thread.set_pkey_access_rights(plan.key(), plan.rights()))
    {
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

    const SUPPORTED_FLAGS: u32 = MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP;
    if !addr.is_multiple_of(PageSize::Size4K as usize) || new_size == 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & !SUPPORTED_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }

    let may_move = flags & MREMAP_MAYMOVE != 0;
    let fixed = flags & MREMAP_FIXED != 0;
    let dont_unmap = flags & MREMAP_DONTUNMAP != 0;
    if fixed && !may_move {
        return Err(AxError::InvalidInput);
    }
    if fixed && !new_addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(AxError::InvalidInput);
    }
    if dont_unmap && !may_move {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    let has_ipc_lock = thread.has_effective_capability(CAP_IPC_LOCK);
    let old_size = checked_align_up_4k(old_size).ok_or(AxError::InvalidInput)?;
    let new_size = checked_align_up_4k(new_size).ok_or(AxError::InvalidInput)?;
    // Linux compares the page-rounded lengths.  Byte lengths which differ
    // inside the same final page are therefore a valid DONTUNMAP duplicate.
    if dont_unmap && old_size != new_size {
        return Err(AxError::InvalidInput);
    }
    remap_user_mapping(
        proc_data,
        has_ipc_lock,
        VirtAddr::from(addr),
        old_size,
        new_size,
        may_move,
        fixed,
        dont_unmap,
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
            | MADV_GUARD_INSTALL
    )
}

/// Applies the subset of `madvise` which can be safely driven against a
/// caller-pinned foreign address space.  This function intentionally has no
/// current-task lookup: `process_madvise` must retain the pidfd image it
/// authorized rather than switching the current address-space context.
pub(crate) fn process_madvise_willneed(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
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
    while cursor < end {
        // Retain at most one exact VMA segment at a time.  This both avoids a
        // fallible unbounded snapshot and leaves the mm unlocked while normal
        // cache replacement may prepare reverse-map eviction reservations.
        let segment = {
            let aspace = aspace_handle.lock();
            let Some(area) = aspace.find_area(cursor) else {
                return Err(AxError::NoMemory);
            };
            if area.start() > cursor {
                return Err(AxError::NoMemory);
            }
            let segment_end = area.end().min(end);
            (
                area.backend().clone(),
                VirtAddrRange::new(cursor, segment_end),
                segment_end,
            )
        };
        let (backend, range, segment_end) = segment;
        // Backing I/O and cache pressure are advisory, but do not turn a full
        // cache into a skipped load: backend prefetch uses the normal cache
        // insertion/replacement path after the mm lock is dropped.
        let _ = backend.prefetch_file_backed(range);
        cursor = segment_end;
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

/// Linux permits file-page eviction only when the caller could write the
/// inode or owns it (including namespace-relative CAP_FOWNER).  A retained
/// mapping lease supplies the exact OFD and mount idmap even after fd close or
/// setns; permission failures make PAGEOUT a no-op for that file VMA rather
/// than an error visible to the caller.
fn file_pageout_authorized(backend: &Backend) -> bool {
    let Some(mapping) = backend.file_mapping() else {
        return false;
    };
    let file = mapping.file();
    let location = file.inner().location();
    let Ok(metadata) = location.metadata() else {
        return false;
    };
    let security = current_vfs_security();
    let idmap = file.vfs_mount_idmap();
    inode_flags::owner_or_capable_with_idmap(&metadata, &security, idmap.as_deref())
        || check_inode_permissions_with_security_and_idmap(
            location,
            &metadata,
            W_OK,
            &security,
            idmap.as_deref(),
        )
        .is_ok()
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
        let file_backed = backend.has_file_cache_backing();
        let file_pageout_allowed = file_backed && file_pageout_authorized(&backend);
        // Linux returns success without touching a shared file VMA when the
        // actor lacks file-page eviction authority. Private mappings may
        // still demote their process-private COW leaves, but may not use that
        // path to evict the retained source inode's cache.
        if file_backed
            && backend
                .file_mapping()
                .is_some_and(|mapping| mapping.sharing() == FileMappingSharing::Shared)
            && !file_pageout_allowed
        {
            return Ok(());
        }
        // PAGEOUT first demotes resident PTEs. Without swap this is the
        // Linux outcome for private anonymous and shmem leaves: retain data
        // and make it reclaim-eligible, rather than falsely discarding it or
        // rejecting an otherwise valid advisory request.
        aspace.cold_resident_pages(range)?;
        // Only shared file PTEs alias an inode cache page. MAP_PRIVATE COW
        // pages have already been demoted above but must not turn PAGEOUT
        // into a source-cache eviction of their original file bytes.
        if file_pageout_allowed && matches!(backend, Backend::File(_)) {
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
const ALIAS_GRANULARITY_CHUNK: usize = PageSize::Size2M as usize;

fn first_alias_granularity_chunk(start: VirtAddr, aspace_base: VirtAddr) -> AxResult<VirtAddr> {
    let align_down =
        |address: VirtAddr| VirtAddr::from(address.as_usize() & !(ALIAS_GRANULARITY_CHUNK - 1));
    let align_up = |address: VirtAddr| {
        address
            .as_usize()
            .checked_add(ALIAS_GRANULARITY_CHUNK - 1)
            .map(|address| VirtAddr::from(address & !(ALIAS_GRANULARITY_CHUNK - 1)))
            .ok_or(AxError::InvalidInput)
    };

    Ok(align_down(start).max(align_up(aspace_base)?))
}

fn alias_granularity_chunk_fits(
    chunk_start: VirtAddr,
    request_end: VirtAddr,
    aspace_end: VirtAddr,
) -> bool {
    chunk_start < request_end
        && chunk_start
            .checked_add(ALIAS_GRANULARITY_CHUNK)
            .is_some_and(|chunk_end| chunk_end <= aspace_end)
}

pub(crate) fn ensure_4k_granularity_across_aliases(
    aspace_handle: &Arc<axsync::Mutex<AddrSpace>>,
    start: VirtAddr,
    length: usize,
) -> AxResult {
    if length == 0 {
        return Ok(());
    }
    let end = start.checked_add(length).ok_or(AxError::InvalidInput)?;
    let (aspace_base, aspace_end) = {
        let aspace = aspace_handle.lock();
        (aspace.base(), aspace.end())
    };
    let mut cursor = first_alias_granularity_chunk(start, aspace_base)?;
    while alias_granularity_chunk_fits(cursor, end, aspace_end) {
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
                            .query_mapped(cursor)
                            .is_ok_and(|(_, _, size)| size == PageSize::Size2M))
                    .then_some((pages, start_index))
                }
                _ => None,
            }
        };
        if let Some((pages, start_index)) = compound {
            demote_shared_folio_across_aliases(aspace_handle, pages, start_index)?;
        } else {
            let mut aspace = aspace_handle.lock();
            // This helper is used before several permission and remap
            // transactions.  Close its own probe-to-mutation gap rather than
            // relying on every caller to remember a second fence check.
            if let Some(retry) =
                aspace.file_eviction_retry_for_range(cursor, ALIAS_GRANULARITY_CHUNK)
            {
                drop(aspace);
                retry.wait()?;
                continue;
            }
            aspace.ensure_4k_granularity(cursor, ALIAS_GRANULARITY_CHUNK)?;
        }
        cursor = cursor
            .checked_add(ALIAS_GRANULARITY_CHUNK)
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
        .filter_map(|alias| {
            alias
                .revalidate()
                .map(|aspace| (alias.address_space_id(), aspace))
        })
        .collect::<Vec<_>>();
    let target_id = target.lock().address_space_id();
    if !participants
        .iter()
        .any(|(_, aspace)| Arc::ptr_eq(aspace, target))
    {
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

    demote_shared_folio_locked(&pages, start_index, &mut guards)
}

/// Demotes one promoted shmem folio while the caller owns the backing's alias
/// mutation reservation and every participant mm lock.
fn demote_shared_folio_locked(
    pages: &Arc<SharedPages>,
    start_index: usize,
    guards: &mut [axsync::MutexGuard<'_, AddrSpace>],
) -> AxResult<()> {
    // Snapshot every fallible backing resource before touching any PTE.  The
    // old 4 KiB frames remain folio-owned until the final commit.
    let frames = pages.demote_4k_folio_frames(start_index)?;
    let mut plans = Vec::new();
    for (guard_index, guard) in guards.iter().enumerate() {
        for alias_start in guard.shared_folio_alias_starts(&pages, start_index)? {
            let flags =
                guard.preflight_shared_folio_demotion_2m(alias_start, &pages, start_index)?;
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

    for (protected, &(guard_index, alias_start, flags)) in plans.iter().enumerate() {
        if let Err(error) =
            guards[guard_index].write_protect_shared_folio_demotion_2m(alias_start, flags)
        {
            for &(rollback_guard, rollback_start, rollback_flags) in plans[..protected].iter().rev()
            {
                guards[rollback_guard].restore_shared_folio_demotion_pmd_permissions(
                    rollback_start,
                    rollback_flags,
                )?;
            }
            return Err(error);
        }
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
                    guards[rollback_guard].restore_shared_folio_demotion_pmd_permissions(
                        rollback_start,
                        rollback_flags,
                    )?;
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

/// Creates a sparse anonymous-shmem hole across every address space alias.
/// Alias publication is frozen, every participant mm is locked in stable ID
/// order, all PTEs are detached and globally invalidated, and only then are
/// resident frames returned to the allocator.  Thus no CPU can retain a
/// writable translation to a freed page and a concurrent fault cannot
/// repopulate the backing between invalidation and hole publication.
fn remove_anonymous_shared_across_aliases(
    target: &Arc<axsync::Mutex<AddrSpace>>,
    pages: Arc<SharedPages>,
    offset: usize,
    length: usize,
) -> AxResult<()> {
    let end = offset.checked_add(length).ok_or(AxError::InvalidInput)?;
    let (_mutation, aliases) = crate::mm::reserve_alias_mutation(pages.backing_key());
    let mut participants = aliases
        .into_iter()
        .filter_map(|alias| {
            alias
                .revalidate()
                .map(|aspace| (alias.address_space_id(), aspace))
        })
        .collect::<Vec<_>>();
    let target_id = target.lock().address_space_id();
    if !participants
        .iter()
        .any(|(_, aspace)| Arc::ptr_eq(aspace, target))
    {
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

    // Keep the same alias reservation and mm lock set from demotion through
    // hole publication. A concurrent collapse therefore cannot recreate a
    // folio after demotion but before PTE detachment.
    let folio_bytes = PageSize::Size2M as usize;
    let mut folio_offset = offset / folio_bytes * folio_bytes;
    while folio_offset < end {
        let start_index = folio_offset / PAGE_SIZE_4K;
        if pages.has_4k_folio(start_index) {
            demote_shared_folio_locked(&pages, start_index, &mut guards)?;
        }
        folio_offset = folio_offset
            .checked_add(folio_bytes)
            .ok_or(AxError::InvalidInput)?;
    }

    // Allocate and preflight every participant plan before the first PTE is
    // changed. After this boundary drain_mapped_leaves is a validated,
    // allocation-reserved operation.
    let mut plans = Vec::new();
    plans
        .try_reserve_exact(guards.len())
        .map_err(|_| AxError::NoMemory)?;
    for guard in &guards {
        let ranges = guard.shared_backing_alias_ranges(&pages, offset, length)?;
        guard.preflight_shared_backing_detach(&ranges)?;
        plans.push(ranges);
    }

    let mut changed = Vec::new();
    changed
        .try_reserve_exact(guards.len())
        .map_err(|_| AxError::NoMemory)?;
    for (guard, ranges) in guards.iter_mut().zip(&plans) {
        changed.push(guard.detach_preflighted_shared_backing_ranges(ranges)?);
    }
    for (guard, changed) in guards.iter_mut().zip(changed) {
        if changed {
            drop(guard.synchronize_tlb_after_mutation());
        }
    }
    pages.remove_range(offset, length)
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
    // A huge collapse replaces 4 KiB alias PTEs with one compound mapping.
    // Do not race a pageout fence; restart participant discovery after the
    // terminal cache edge so every alias set is revalidated.
    let retry = {
        let aspace = aspace_handle.lock();
        aspace.file_eviction_retry_for_range(VirtAddr::from(addr), length)
    };
    if let Some(retry) = retry {
        retry.wait()?;
        return process_madvise_collapse(aspace_handle, addr, length);
    }
    let shared_key = {
        let aspace = aspace_handle.lock();
        // The first probe above is deliberately lock-external while waiting.
        // Recheck after acquiring the lock used for participant discovery: an
        // eviction can publish in that gap, and collapse would otherwise
        // replace its write-protected 4KiB aliases with a new huge leaf.
        if let Some(retry) = aspace.file_eviction_retry_for_range(VirtAddr::from(addr), length) {
            drop(aspace);
            retry.wait()?;
            return process_madvise_collapse(aspace_handle, addr, length);
        }
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
            .filter_map(|alias| {
                alias
                    .revalidate()
                    .map(|aspace| (alias.address_space_id(), aspace))
            })
            .collect::<Vec<_>>();
        let target_id = aspace_handle.lock().address_space_id();
        if !participants
            .iter()
            .any(|(_, alias)| Arc::ptr_eq(alias, aspace_handle))
        {
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

fn restore_shared_collapse_source_permissions(
    guards: &mut [axsync::MutexGuard<'_, AddrSpace>],
    target_index: usize,
    start: VirtAddr,
    target_flags: MappingFlags,
    redirects: &[Vec<SharedFolioPteRedirect>],
    pmd_sources: &[(usize, VirtAddr, MappingFlags)],
) -> AxResult {
    // Restoration is best-effort across every participant even if one page
    // table reports an invariant failure.  Returning after the first error
    // would strand unrelated aliases read-only and make a later retry observe
    // a topology created by only half of this transaction.
    let mut first_error = None;
    for &(guard_index, pmd_start, pmd_flags) in pmd_sources {
        if let Err(error) =
            guards[guard_index].restore_shared_folio_demotion_pmd_permissions(pmd_start, pmd_flags)
        {
            first_error.get_or_insert(error);
        }
    }
    for (guard_index, redirect) in redirects.iter().enumerate() {
        if let Err(error) = guards[guard_index].restore_shared_folio_redirect_permissions(redirect)
        {
            first_error.get_or_insert(error);
        }
    }
    if let Err(error) =
        guards[target_index].restore_shared_folio_permissions_2m(start, target_flags)
    {
        first_error.get_or_insert(error);
    }
    first_error.map_or(Ok(()), Err)
}

/// Rolls a promoted shared-folio transaction back without exposing a writable
/// old alias while the folio is copied into its retained base pages.
fn rollback_shared_collapse_after_promotion(
    guards: &mut [axsync::MutexGuard<'_, AddrSpace>],
    target_index: usize,
    start: VirtAddr,
    target_flags: MappingFlags,
    redirects: &[Vec<SharedFolioPteRedirect>],
    pmd_sources: &[(usize, VirtAddr, MappingFlags)],
    redirected_guards: usize,
    target_replacement: Option<SharedFolioPteReplacement>,
    pmd_published: &mut Vec<(usize, SharedFolioDemotionReplacement)>,
    pages: &Arc<SharedPages>,
    start_index: usize,
    newly_promoted: bool,
) -> AxResult {
    let mut first_error = None;

    // First put every published translation back on the old backing, but
    // retain the write revocation established before promotion.
    for rollback in (0..redirected_guards).rev() {
        if let Err(error) = guards[rollback].rollback_shared_folio_redirects(&redirects[rollback]) {
            first_error.get_or_insert(error);
        }
    }
    for (guard_index, replacement) in pmd_published.drain(..).rev() {
        if let Err(error) =
            guards[guard_index].rollback_shared_folio_demotion_2m_protected(replacement)
        {
            first_error.get_or_insert(error);
        }
    }
    if let Some(replacement) = target_replacement {
        if let Err(error) = guards[target_index].rollback_shared_folio_collapse_2m(replacement) {
            first_error.get_or_insert(error);
        }
    }

    // Close every stale writable/new-backing translation before changing
    // backing ownership.  If page-table rollback itself failed, fail closed:
    // the folio still owns both representations and all admitted aliases stay
    // read-only instead of copying through an unknown live mapping.
    for guard in guards.iter_mut() {
        drop(guard.synchronize_tlb_after_mutation());
    }
    if let Some(error) = first_error {
        return Err(error);
    }

    if newly_promoted {
        pages.demote_4k_folio(start_index)?;
    }

    // Only the backing transition above makes the old frames authoritative
    // again.  WRITE restoration is therefore the final rollback phase.
    restore_shared_collapse_source_permissions(
        guards,
        target_index,
        start,
        target_flags,
        redirects,
        pmd_sources,
    )
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
            cursor = cursor
                .checked_add(PageSize::Size2M as usize)
                .ok_or(AxError::InvalidInput)?;
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
        // Only the requesting complete PMD becomes huge.  Other aliases are
        // prepared as P1 redirects, including aliases in another mm and VMA
        // fragments which cover merely a subset of this backing folio.
        if !guards[target_index].forced_thp_collapse_allowed(start, PageSize::Size2M as usize) {
            return Err(AxError::InvalidInput);
        }
        let existing_folio = pages.has_4k_folio(start_index);
        let target_is_existing_pmd = if existing_folio {
            let folio = pages.paddr_at(start_index)?;
            matches!(guards[target_index].page_table().query(start), Ok((paddr, _, PageSize::Size2M)) if paddr == folio)
        } else {
            false
        };
        if target_is_existing_pmd {
            // This exact target is already the sole PMD for the folio.  Its
            // P1 aliases remain valid redirects and must not be sent through
            // the legacy all-PMD demotion path merely to collapse again.
            cursor = cursor
                .checked_add(PageSize::Size2M as usize)
                .ok_or(AxError::InvalidInput)?;
            continue;
        }
        let target_flags =
            guards[target_index].preflight_shared_folio_collapse_2m(start, &pages, start_index)?;
        let mut pmd_plans = Vec::new();
        pmd_plans
            .try_reserve_exact(guards.len())
            .map_err(|_| AxError::NoMemory)?;
        for (index, guard) in guards.iter().enumerate() {
            pmd_plans.push(guard.prepare_shared_folio_pmd_redirects_except(
                &pages,
                start_index,
                (index == target_index).then_some(start),
            )?);
        }
        let mut redirects = Vec::new();
        redirects
            .try_reserve_exact(guards.len())
            .map_err(|_| AxError::NoMemory)?;
        for (guard_index, guard) in guards.iter().enumerate() {
            let exclude_target = (guard_index == target_index).then_some(start);
            redirects.push(guard.preflight_shared_folio_redirects_2m(
                &pages,
                start_index,
                exclude_target,
            )?);
        }
        // Reserve the new folio, sparse source frames and backing metadata
        // before any PTE permission becomes visible.  From the first write
        // protection onward, publication is allocation-free; dropping this
        // object on every pre-publication failure returns its temporary RAM.
        let mut prepared_folio = (!existing_folio)
            .then(|| pages.prepare_4k_folio_promotion(start_index))
            .transpose()?;
        let existing_folio_address = existing_folio
            .then(|| pages.paddr_at(start_index))
            .transpose()?;
        let pmd_plan_count = pmd_plans.iter().try_fold(0usize, |count, plans| {
            count.checked_add(plans.len()).ok_or(AxError::NoMemory)
        })?;
        let mut pmd_sources = Vec::new();
        pmd_sources
            .try_reserve_exact(pmd_plan_count)
            .map_err(|_| AxError::NoMemory)?;
        for (guard_index, plans) in pmd_plans.iter().enumerate() {
            for plan in plans {
                pmd_sources.push((guard_index, plan.start, plan.flags));
            }
        }
        let mut pmd_published = Vec::new();
        pmd_published
            .try_reserve_exact(pmd_plan_count)
            .map_err(|_| AxError::NoMemory)?;
        if let Err(error) =
            guards[target_index].write_protect_shared_folio_collapse_2m(start, target_flags)
        {
            return Err(error);
        }
        for (guard_index, redirect) in redirects.iter().enumerate() {
            if let Err(error) = guards[guard_index].write_protect_shared_folio_redirects(redirect) {
                if let Err(rollback_error) = restore_shared_collapse_source_permissions(
                    guards,
                    target_index,
                    start,
                    target_flags,
                    &redirects,
                    &pmd_sources,
                ) {
                    return Err(rollback_error);
                }
                return Err(error);
            }
        }
        for &(guard_index, pmd_start, pmd_flags) in &pmd_sources {
            if let Err(error) =
                guards[guard_index].write_protect_shared_folio_demotion_2m(pmd_start, pmd_flags)
            {
                if let Err(rollback_error) = restore_shared_collapse_source_permissions(
                    guards,
                    target_index,
                    start,
                    target_flags,
                    &redirects,
                    &pmd_sources,
                ) {
                    return Err(rollback_error);
                }
                return Err(error);
            }
        }
        // Every participating mm receives a grace after write protection and
        // before the backing copy.  This closes stale writable translations.
        for guard in guards.iter_mut() {
            drop(guard.synchronize_tlb_after_mutation());
        }
        let folio = if let Some(folio) = existing_folio_address {
            folio
        } else {
            pages.commit_4k_folio_promotion(
                prepared_folio
                    .take()
                    .expect("new shmem folio lost its prepared allocation"),
            )
        };
        let target_replacement =
            match guards[target_index].publish_shared_folio_collapse_2m(start, folio, target_flags)
            {
                Ok(replacement) => replacement,
                Err(error) => {
                    rollback_shared_collapse_after_promotion(
                        guards,
                        target_index,
                        start,
                        target_flags,
                        &redirects,
                        &pmd_sources,
                        0,
                        None,
                        &mut pmd_published,
                        &pages,
                        start_index,
                        !existing_folio,
                    )?;
                    return Err(error);
                }
            };
        for (guard_index, plans) in pmd_plans.iter_mut().enumerate() {
            for plan in plans {
                match guards[guard_index].publish_shared_folio_pmd_redirect(plan, folio) {
                    Ok(replacement) => pmd_published.push((guard_index, replacement)),
                    Err(error) => {
                        rollback_shared_collapse_after_promotion(
                            guards,
                            target_index,
                            start,
                            target_flags,
                            &redirects,
                            &pmd_sources,
                            0,
                            Some(target_replacement),
                            &mut pmd_published,
                            &pages,
                            start_index,
                            !existing_folio,
                        )?;
                        return Err(error);
                    }
                }
            }
        }
        let mut redirected = 0usize;
        for (guard_index, redirect) in redirects.iter().enumerate() {
            if let Err(error) = guards[guard_index].publish_shared_folio_redirects(redirect, folio)
            {
                rollback_shared_collapse_after_promotion(
                    guards,
                    target_index,
                    start,
                    target_flags,
                    &redirects,
                    &pmd_sources,
                    redirected,
                    Some(target_replacement),
                    &mut pmd_published,
                    &pages,
                    start_index,
                    !existing_folio,
                )?;
                return Err(error);
            }
            redirected += 1;
        }
        // Target PMD and every non-target P1 redirect are now read-only and
        // name the promoted folio.  Wait before releasing retained P1 data,
        // then restore each original writable contract.
        for guard in guards.iter_mut() {
            drop(guard.synchronize_tlb_after_mutation());
        }
        guards[target_index].commit_shared_folio_collapse_2m(target_replacement);
        let mut restore_error = None;
        for (guard_index, replacement) in pmd_published {
            if let Err(error) = guards[guard_index]
                .restore_shared_folio_permissions_2m(replacement.start, replacement.flags)
            {
                restore_error.get_or_insert(error);
            }
        }
        if let Err(error) =
            guards[target_index].restore_shared_folio_demotion_pmd_permissions(start, target_flags)
        {
            restore_error.get_or_insert(error);
        }
        for (guard_index, redirect) in redirects.iter().enumerate() {
            if let Err(error) =
                guards[guard_index].restore_shared_folio_redirect_permissions(redirect)
            {
                restore_error.get_or_insert(error);
            }
        }
        if let Some(error) = restore_error {
            return Err(error);
        }
        cursor = cursor
            .checked_add(PageSize::Size2M as usize)
            .ok_or(AxError::InvalidInput)?;
    }
    Ok(())
}

fn process_madvise_collapse_locked(aspace: &mut AddrSpace, addr: usize, length: usize) -> AxResult {
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

pub(super) fn madvise_behavior_valid(advice: u32) -> bool {
    matches!(advice, MADV_NORMAL | MADV_RANDOM | MADV_SEQUENTIAL | MADV_WILLNEED |
        MADV_DONTNEED | MADV_FREE | MADV_REMOVE | MADV_DONTFORK | MADV_DOFORK |
        MADV_MERGEABLE | MADV_UNMERGEABLE | MADV_HUGEPAGE | MADV_NOHUGEPAGE |
        MADV_DONTDUMP | MADV_DODUMP | MADV_WIPEONFORK | MADV_KEEPONFORK |
        MADV_COLD | MADV_PAGEOUT | MADV_POPULATE_READ | MADV_POPULATE_WRITE |
        MADV_DONTNEED_LOCKED | MADV_COLLAPSE | MADV_GUARD_INSTALL | MADV_GUARD_REMOVE |
        MADV_HWPOISON | MADV_SOFT_OFFLINE)
}

pub fn sys_madvise(addr: usize, length: usize, advice: u32) -> AxResult<isize> {
    debug!("sys_madvise <= addr: {addr:#x}, length: {length:x}, advice: {advice:#x}");

    if !madvise_behavior_valid(advice) || !addr.is_multiple_of(PageSize::Size4K as usize) {
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

    if advice == MADV_WILLNEED {
        // File-cache population can have to reclaim an unrelated cache page
        // and notify every alias of that page.  The shared helper owns that
        // lock-external retry protocol, so do not retain this mm lock while
        // doing real readahead.  Anonymous mappings remain a validated no-op
        // in Backend::prefetch_file_backed, matching Linux's advisory model.
        drop(aspace);
        process_madvise_willneed(&aspace_handle, addr, length)?;
        return Ok(0);
    }

    if advice == MADV_COLLAPSE {
        drop(aspace);
        process_madvise_collapse(&aspace_handle, addr, length)?;
        return Ok(0);
    }

    if advice == MADV_PAGEOUT {
        let mut work = Vec::new();
        process_madvise_collect_pageout(&mut aspace, addr, length, &mut work)?;
        drop(aspace);
        // Page-cache eviction can synchronously notify every alias, so it
        // must run after dropping the target mm lock.  The collector above
        // has already demoted PTEs and captured exact backend/range pairs.
        for (backend, range) in work {
            backend.pageout_file_pages(range)?;
        }
        return Ok(0);
    }

    if advice == MADV_GUARD_INSTALL {
        // Guard installation is a real VMA overlay, not an advisory success:
        // it discards any resident anonymous pages and turns later faults
        // into access denials until an explicit GUARD_REMOVE.
        aspace.install_madvise_guard(start, length)?;
        return Ok(0);
    }
    if advice == MADV_GUARD_REMOVE {
        aspace.remove_madvise_guard(start, length)?;
        return Ok(0);
    }
    if advice == MADV_HWPOISON {
        if !curr.as_thread().has_effective_capability(CAP_SYS_ADMIN) {
            return Err(AxError::OperationNotPermitted);
        }
        aspace.install_madvise_hwpoison(start, length)?;
        return Ok(0);
    }
    if advice == MADV_SOFT_OFFLINE {
        if !curr.as_thread().has_effective_capability(CAP_SYS_ADMIN) {
            return Err(AxError::OperationNotPermitted);
        }
        // There is no NUMA migration target on x86_64 TheKernel.  Retiring
        // resident pages forces the same allocate-on-next-access migration
        // outcome without poisoning the virtual address.
        aspace.discard_pages(start, length)?;
        return Ok(0);
    }

    if matches!(advice, MADV_POPULATE_READ | MADV_POPULATE_WRITE) {
        let access = if advice == MADV_POPULATE_READ {
            MappingFlags::READ
        } else {
            MappingFlags::WRITE
        };
        // Population can require a page-cache eviction transaction.  Drop
        // this mm lock before reclaim and re-run the complete range/permission
        // check for every retry.
        drop(aspace);
        populate_explicit_with_reclaim(&aspace_handle, start, length, access, |aspace| {
            inspect_madvise_range(aspace, start, length).map(|_| ())
        })?;
        return Ok(0);
    }

    match advice {
        MADV_NORMAL | MADV_RANDOM | MADV_SEQUENTIAL => {
            inspect_madvise_range(&aspace, start, length)?;
            let policy = match advice {
                MADV_NORMAL => MadviseReadahead::Normal,
                MADV_RANDOM => MadviseReadahead::Random,
                MADV_SEQUENTIAL => MadviseReadahead::Sequential,
                _ => unreachable!("madvise readahead behavior was matched above"),
            };
            aspace.set_madvise_readahead(start, length, policy)?;
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
        MADV_POPULATE_READ | MADV_POPULATE_WRITE => unreachable!("handled above"),
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
            if aspace.range_is_locked(start, length) {
                return Err(AxError::ResourceBusy);
            }
            aspace.mark_madvise_free(start, length)?;
            Ok(0)
        }
        MADV_COLD => {
            process_madvise_cold(&mut aspace, addr, length)?;
            Ok(0)
        }
        MADV_DONTNEED_LOCKED => {
            let info = inspect_madvise_range(&aspace, start, length)?;
            if info.has_shared_mapping {
                return Err(AxError::InvalidInput);
            }
            aspace.discard_pages(start, length)?;
            Ok(0)
        }
        MADV_HUGEPAGE | MADV_NOHUGEPAGE => {
            inspect_madvise_range(&aspace, start, length)?;
            aspace.set_madvise_thp(
                start,
                length,
                if advice == MADV_HUGEPAGE {
                    MadviseThp::Huge
                } else {
                    MadviseThp::NoHuge
                },
            )?;
            Ok(0)
        }
        // KSM requires global canonical-frame reverse mappings and a
        // write-fault COW path. Keep these accepted hints separate from THP:
        // they must not claim the now-observable THP policy implementation.
        MADV_MERGEABLE | MADV_UNMERGEABLE => {
            inspect_madvise_range(&aspace, start, length)?;
            Ok(0)
        }
        MADV_DONTDUMP | MADV_DODUMP => {
            inspect_madvise_range(&aspace, start, length)?;
            aspace.set_dontdump(start, length, advice == MADV_DONTDUMP)?;
            Ok(0)
        }
        MADV_REMOVE => {
            let targets = collect_madvise_remove_targets(&aspace, start, length)?;
            let security = current_vfs_security();
            drop(aspace);
            for target in targets {
                match target {
                    MadviseRemoveTarget::File {
                        file,
                        offset,
                        length,
                    } => crate::syscall::punch_hole_file_mapping(&file, &security, offset, length)?,
                    MadviseRemoveTarget::AnonymousShared {
                        pages,
                        offset,
                        length,
                    } => remove_anonymous_shared_across_aliases(
                        &aspace_handle,
                        pages,
                        offset,
                        length,
                    )?,
                }
            }
            Ok(0)
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
    msync_address_space(&aspace_handle, addr, length, flags)
}

fn msync_address_space(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    addr: usize,
    length: usize,
    flags: u32,
) -> AxResult<isize> {
    const PAGE_SIZE: usize = PageSize::Size4K as usize;
    let length = checked_align_up(length, PAGE_SIZE).ok_or(AxError::NoMemory)?;
    addr.checked_add(length).ok_or(AxError::NoMemory)?;
    let fail_on_first_unmapped = flags == MS_ASYNC;
    let (backends, saw_unmapped) = {
        let aspace = aspace_handle.lock();
        if length > 0 {
            let start = VirtAddr::from(addr);
            let (_, saw_unmapped) =
                aspace.sync_backends_in_range(start, length, fail_on_first_unmapped)?;
            if flags & MS_INVALIDATE != 0 && aspace.range_is_locked(start, length) {
                return Err(LinuxError::EBUSY.into());
            }
            // Linux only calls vfs_fsync_range for MS_SYNC VM_SHARED file
            // mappings.  In particular, MS_ASYNC starts no I/O and a private
            // file COW mapping must not flush its source inode merely because
            // it intersects an msync range.
            let mut backends = Vec::new();
            if flags & MS_SYNC != 0 {
                let end = start + length;
                // Holes affect the final errno, not the remaining sync
                // work. Walk every intersecting VMA and clamp its own
                // overlap so a leading/interior hole cannot stop writeback.
                for area in aspace.areas_overlapping(VirtAddrRange::new(start, end)) {
                    let overlap_start = area.start().max(start);
                    if area
                        .backend()
                        .file_mapping()
                        .is_some_and(|lease| lease.sharing() == FileMappingSharing::Shared)
                    {
                        backends.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                        let overlap_end = area.end().min(end);
                        let offset = area
                            .backend()
                            .file_mapping()
                            .and_then(|lease| lease.file_offset_at(overlap_start))
                            .ok_or(AxError::BadState)?;
                        backends.push((
                            area.backend().clone(),
                            offset,
                            overlap_end.sub_addr(overlap_start) as u64,
                        ));
                    }
                }
            }
            (backends, saw_unmapped)
        } else {
            (Vec::new(), false)
        }
    };

    if flags & MS_SYNC != 0 {
        for (backend, offset, length) in backends {
            backend.sync_range(offset, length, false)?;
        }
    }

    // MS_INVALIDATE's only msync-visible effect is the VM_LOCKED rejection.
    // Recheck after the lock-external filesystem operation so a concurrent
    // mlock cannot race a successful snapshot into an incorrectly successful
    // invalidate request.
    if flags & MS_INVALIDATE != 0 && length > 0 {
        let aspace = aspace_handle.lock();
        if aspace.range_is_locked(VirtAddr::from(addr), length) {
            return Err(LinuxError::EBUSY.into());
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
    if !has_ipc_lock && proc_data.rlim.read()[RLIMIT_MEMLOCK].current == 0 {
        return Err(limit_error);
    }
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
    let (start, length) = validate_page_aligned_range(addr, length)?;
    loop {
        let mut aspace = aspace_handle.lock();
        if let Some(retry) = aspace.file_eviction_retry_for_range(start, length) {
            drop(aspace);
            retry.wait()?;
            continue;
        }
        check_mlock_range_limit(proc_data, has_ipc_lock, &aspace, start, length)?;
        apply_lock_prefix(&mut aspace, start, length, true)?;
        drop(aspace);
        if flags & MLOCK_ONFAULT == 0 {
            populate_explicit_with_reclaim(&aspace_handle, start, length,
                MappingFlags::empty(), |_| Ok(()))?;
        }
        return Ok(0);
    }
}

// Linux apply_vma_lock_flags commits each VMA before encountering a hole.
// Keep the mm lock across this walk so its successful prefix is unambiguous.
fn apply_lock_prefix(aspace: &mut AddrSpace, start: VirtAddr, length: usize, enabled: bool) -> AxResult {
    let end = start.checked_add(length).ok_or(AxError::InvalidInput)?;
    let mut cursor = start;
    while cursor < end {
        let area = aspace.find_area(cursor).ok_or(AxError::NoMemory)?;
        if area.start() > cursor {
            return Err(AxError::NoMemory);
        }
        let next = area.end().min(end);
        aspace.set_locked(cursor, next.sub_addr(cursor), enabled)?;
        cursor = next;
    }
    Ok(())
}

pub fn sys_munlock(addr: usize, length: usize) -> AxResult<isize> {
    let curr = current();
    let aspace_handle = curr.as_thread().proc_data.aspace();
    let (start, length) = validate_page_aligned_range(addr, length)?;
    apply_lock_prefix(&mut aspace_handle.lock(), start, length, false)?;
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
    if flags & MCL_CURRENT != 0 {
        let mut aspace = aspace_handle.lock();
        check_mlockall_current_limit(proc_data, has_ipc_lock, &aspace)?;
        let ranges: Vec<_> = if flags & MCL_ONFAULT == 0 {
            aspace
                .areas()
                .map(|area| (area.start(), area.size()))
                .collect()
        } else {
            Vec::new()
        };
        aspace.lock_current_mappings();
        aspace.set_lock_future_mappings(flags & MCL_FUTURE != 0, flags & MCL_ONFAULT != 0);
        drop(aspace);
        if flags & MCL_ONFAULT == 0 {
            for (start, size) in ranges {
                let _ = populate_explicit_with_reclaim(
                    &aspace_handle,
                    start,
                    size,
                    MappingFlags::empty(),
                    |aspace| {
                        aspace
                            .find_area(start)
                            .filter(|area| {
                                start.checked_add(size).is_some_and(|end| area.end() >= end)
                            })
                            .map(|_| ())
                            .ok_or(AxError::NoMemory)
                    },
                );
            }
        }
        return Ok(0);
    } else {
        check_mlockall_future_limit(proc_data, has_ipc_lock)?;
    }

    let mut aspace = aspace_handle.lock();
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
    fn pkey_alloc_rejects_full_width_invalid_arguments_before_task_access() {
        for (flags, rights) in [
            (1usize << 32, 0),
            (1, 0),
            (0, 1usize << 32),
            (0, 4),
            (0, usize::MAX),
        ] {
            // No current process is needed to reject these arguments, even
            // on a host without enabled PKU or an available protection key.
            assert_eq!(sys_pkey_alloc(flags, rights), Err(AxError::InvalidInput));
        }
    }

    #[test]
    fn alias_granularity_skips_partial_first_address_space_window() {
        let base = VirtAddr::from(0x1000);
        let aspace_end = VirtAddr::from(0x400000);
        let start = VirtAddr::from(0xc3000);
        let request_end = VirtAddr::from(0xc4000);
        let first = first_alias_granularity_chunk(start, base).unwrap();

        assert_eq!(first, VirtAddr::from(ALIAS_GRANULARITY_CHUNK));
        assert!(!alias_granularity_chunk_fits(
            first,
            request_end,
            aspace_end
        ));
    }

    #[test]
    fn alias_granularity_checks_first_full_chunk_crossed_by_request() {
        let base = VirtAddr::from(0x1000);
        let first = first_alias_granularity_chunk(VirtAddr::from(0xc3000), base).unwrap();

        assert!(alias_granularity_chunk_fits(
            first,
            VirtAddr::from(ALIAS_GRANULARITY_CHUNK + PAGE_SIZE_4K),
            VirtAddr::from(ALIAS_GRANULARITY_CHUNK * 2),
        ));
        assert!(!alias_granularity_chunk_fits(
            first,
            VirtAddr::from(ALIAS_GRANULARITY_CHUNK + PAGE_SIZE_4K),
            VirtAddr::from(ALIAS_GRANULARITY_CHUNK + PAGE_SIZE_4K),
        ));
    }

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
    fn process_madvise_pageout_collects_only_file_backed_eviction_work() {
        let base = VirtAddr::from(0x4000);
        let mut aspace = AddrSpace::new_empty(base, PAGE_SIZE_4K * 4).unwrap();
        aspace
            .map(
                base,
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                Backend::new_alloc(base, PageSize::Size4K),
            )
            .unwrap();

        let mut work = Vec::new();
        // Anonymous mappings have no swap target. PAGEOUT succeeds by
        // demoting their resident state, but must not fabricate file-cache
        // eviction work.
        assert_eq!(
            process_madvise_collect_pageout(&mut aspace, 0x4000, PAGE_SIZE_4K, &mut work),
            Ok(())
        );
        assert!(work.is_empty());
        // Linux accepts a zero-length advisory range without touching the
        // VMA layout or scheduling any writeback.
        assert_eq!(
            process_madvise_collect_pageout(&mut aspace, 0x4000, 0, &mut work),
            Ok(())
        );
        assert!(work.is_empty());
    }

    #[test]
    fn process_madvise_pageout_failure_does_not_publish_partial_work() {
        let base = VirtAddr::from(0x4000);
        let mut aspace = AddrSpace::new_empty(base, PAGE_SIZE_4K * 4).unwrap();
        let mut work = Vec::new();

        // An unmapped interval is an immediate ENOMEM-style failure and may
        // not leave a stale backend/range pair for the caller to evict.
        assert_eq!(
            process_madvise_collect_pageout(&mut aspace, 0x4000, PAGE_SIZE_4K, &mut work),
            Err(AxError::NoMemory)
        );
        assert!(work.is_empty());
        assert_eq!(
            process_madvise_collect_pageout(&mut aspace, 0x4001, PAGE_SIZE_4K, &mut work),
            Err(AxError::InvalidInput)
        );
        assert!(work.is_empty());
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
        assert_eq!(
            collapse_full_pmd_range(unit + PAGE_SIZE_4K, unit - PAGE_SIZE_4K),
            Ok(None)
        );
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
        assert_eq!(
            preflight_mprotect_geometry(0x4000, 0, usize::MAX),
            Err(AxError::InvalidInput)
        );
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
                axfs_ng_vfs::FsName::new(name.as_bytes()),
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

    // A disk-like cache provider backed by an isolated byte-storage inode.
    // Unlike tmpfs, its cache must write through to the backing node on sync.
    struct MsyncBackingFile {
        this: alloc::sync::Weak<Self>,
        backing: axfs_ng_vfs::Location,
    }

    impl MsyncBackingFile {
        fn node(&self) -> &axfs_ng_vfs::FileNode {
            self.backing.entry().as_file().expect("test backing is a regular file")
        }
    }

    impl axfs_ng_vfs::FilesystemOps for MsyncBackingFile {
        fn name(&self) -> &str { "msync-writeback-test" }
        fn root_dir(&self) -> axfs_ng_vfs::DirEntry {
            axfs_ng_vfs::DirEntry::new_file(
                axfs_ng_vfs::FileNode::new(self.this.upgrade().unwrap()),
                NodeType::RegularFile, axfs_ng_vfs::Reference::root())
        }
        fn stat(&self) -> axfs_ng_vfs::VfsResult<axfs_ng_vfs::StatFs> {
            self.node().filesystem().stat()
        }
    }
    impl axfs_ng_vfs::NodeOps for MsyncBackingFile {
        fn inode(&self) -> u64 { self.node().inode() }
        fn metadata(&self) -> axfs_ng_vfs::VfsResult<axfs_ng_vfs::Metadata> { self.node().metadata() }
        fn update_metadata(&self, update: axfs_ng_vfs::MetadataUpdate) -> axfs_ng_vfs::VfsResult<()> {
            self.node().update_metadata(update)
        }
        fn filesystem(&self) -> &dyn axfs_ng_vfs::FilesystemOps { self }
        fn sync(&self, data_only: bool) -> axfs_ng_vfs::VfsResult<()> { self.node().sync(data_only) }
        fn persistent_user_data(&self) -> Option<&axfs_ng_vfs::NodeUserData> {
            self.node().persistent_user_data()
        }
        fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> { self }
    }
    impl axpoll::Pollable for MsyncBackingFile {
        fn poll(&self) -> axpoll::IoEvents { axpoll::IoEvents::READABLE | axpoll::IoEvents::WRITABLE }
        fn register<'a>(&'a self, _: &mut core::task::Context<'_>, _: axpoll::IoEvents)
            -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
            axpoll::PollRegistration::empty()
        }
    }
    impl axfs_ng_vfs::FileNodeOps for MsyncBackingFile {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> axfs_ng_vfs::VfsResult<usize> {
            self.node().read_at(buf, offset)
        }
        fn write_at(&self, buf: &[u8], offset: u64) -> axfs_ng_vfs::VfsResult<usize> {
            self.node().write_at(buf, offset)
        }
        fn set_len(&self, len: u64) -> axfs_ng_vfs::VfsResult<()> { self.node().set_len(len) }
        fn append(&self, buf: &[u8]) -> axfs_ng_vfs::VfsResult<(usize, u64)> { self.node().append(buf) }
        fn set_symlink(&self, target: &axfs_ng_vfs::FsPath) -> axfs_ng_vfs::VfsResult<()> {
            self.node().set_symlink(target)
        }
    }

    #[test]
    fn msync_flushes_shared_file_ranges_after_holes_before_reporting_enomem() {
        let _context = crate::test_support::scheduler_test_context();
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let location = mount.root_location().create(
            axfs_ng_vfs::FsName::new(b"msync-holes"),
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o600),
        ).unwrap();
        let backing = location;
        let provider = Arc::new_cyclic(|this| MsyncBackingFile { this: this.clone(), backing });
        let disk = axfs_ng_vfs::Filesystem::new(provider);
        let location = Mountpoint::new_root(&disk).root_location();
        let node = location.entry().as_file().unwrap();
        node.set_len((PAGE_SIZE_4K * 6) as u64).unwrap();
        let cache = CachedFile::get_or_create(location.clone());
        for page in 0..6 {
            cache.write_at_slice(&[page as u8 + 1], (page * PAGE_SIZE_4K) as u64).unwrap();
        }
        let description = FileDescription::new(Arc::new(File::new(axfs::File::new(
            FileBackend::Cached(cache.clone()),
            FileFlags::READ | FileFlags::WRITE,
        )))).unwrap();
        let file = FileHandle::<dyn FileLike>::from_description_for_test(description)
            .downcast::<File>().unwrap();
        let owner = UserNamespace::try_new_root().unwrap();
        let base = VirtAddr::from(0x4000);
        let aspace = Arc::new(Mutex::new(AddrSpace::new_empty(base, PAGE_SIZE_4K * 6).unwrap()));
        for (page, pages, sharing) in [
            (0, 1, FileMappingSharing::Shared),
            (2, 2, FileMappingSharing::Shared),
            (4, 1, FileMappingSharing::Private),
        ] {
            let start = base + page * PAGE_SIZE_4K;
            let lease = FileMappingLease::new(
                file.clone(), owner.clone(), start, (page * PAGE_SIZE_4K) as u64,
                MappingFlags::USER | MappingFlags::READ,
                MappingFlags::READ | MappingFlags::WRITE,
                sharing,
            );
            let backend = if sharing == FileMappingSharing::Shared {
                Backend::new_file(
                    start, cache.clone(), FileFlags::READ | FileFlags::WRITE,
                    page * PAGE_SIZE_4K, Some((PAGE_SIZE_4K * 6) as u64), &aspace,
                ).unwrap()
            } else {
                Backend::new_alloc(start, PageSize::Size4K)
            }.with_file_mapping(lease);
            aspace.lock().map(
                start, pages * PAGE_SIZE_4K, MappingFlags::USER | MappingFlags::READ,
                false, backend,
            ).unwrap();
        }
        // Read the backing node directly, bypassing the dirty page cache.
        // A cached read could conceal missing writeback after a hole.
        let mut byte = [0];
        for page in 0..6 {
            node.read_at(&mut byte, (page * PAGE_SIZE_4K) as u64).unwrap();
            assert_eq!(byte, [0]);
        }
        assert_eq!(
            msync_address_space(&aspace, base.as_usize(), PAGE_SIZE_4K * 6, MS_SYNC),
            Err(AxError::NoMemory),
        );
        for page in 0..6 {
            node.read_at(&mut byte, (page * PAGE_SIZE_4K) as u64).unwrap();
            let expected = if matches!(page, 0 | 2 | 3) { page as u8 + 1 } else { 0 };
            assert_eq!(byte, [expected], "backing page {page}");
        }
        cache.write_at_slice(&[42], (PAGE_SIZE_4K * 2) as u64).unwrap();
        cache.write_at_slice(&[43], (PAGE_SIZE_4K * 3) as u64).unwrap();
        let leading_hole = (base + PAGE_SIZE_4K).as_usize();
        assert_eq!(
            msync_address_space(&aspace, leading_hole, PAGE_SIZE_4K * 2, MS_ASYNC),
            Err(AxError::NoMemory),
        );
        node.read_at(&mut byte, (PAGE_SIZE_4K * 2) as u64).unwrap();
        assert_eq!(byte, [3]);
        assert_eq!(
            msync_address_space(&aspace, leading_hole, PAGE_SIZE_4K * 2, MS_SYNC),
            Err(AxError::NoMemory),
        );
        node.read_at(&mut byte, (PAGE_SIZE_4K * 2) as u64).unwrap();
        assert_eq!(byte, [42]);
        node.read_at(&mut byte, (PAGE_SIZE_4K * 3) as u64).unwrap();
        assert_eq!(byte, [4], "out-of-range suffix must stay dirty");
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
                axfs_ng_vfs::FsName::new(b"shared-writable-dedup"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o6755),
            )
            .unwrap();
        let second = second_mount
            .root_location()
            .lookup_no_follow(axfs_ng_vfs::FsName::new(b"shared-writable-dedup"))
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
