//! Linux-visible mremap planning and address-space transactions.

use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::{MappingFlags, PageSize};
use linux_raw_sys::general::RLIMIT_MEMLOCK;
use memory_addr::{MemoryAddr, VirtAddr, VirtAddrRange};
use memory_set::MappingLineage;
use thekernel_linux_mm::{MemlockLimit, MemlockPlan, PageRange as LinuxPageRange, RemapGeometry};

use crate::{
    mm::{
        AddrSpace, Backend, BackendOps, DeferredUffdWake, ExistingLineageMapError,
        LockExternalUffdOutcome, check_rlimit_as_growth, check_rlimit_as_replacement,
        checked_align_up,
    },
    syscall::{
        ensure_4k_granularity_across_aliases,
        ipc::{
            SysvMremapDuplicateAdmission, prepare_sysv_mremap_duplicate_admission,
            shm_attachment_record_by_finalizer_identity_in_namespace,
        },
    },
    task::ProcessData,
};

#[derive(Clone)]
struct RemapSegment {
    start: VirtAddr,
    size: usize,
    flags: MappingFlags,
    backend: Backend,
    lineage: MappingLineage,
}

#[derive(Clone, Copy)]
struct SysvDuplicateSource {
    finalizer_identity: usize,
    source_start: VirtAddr,
    object_offset: usize,
    destination_start: VirtAddr,
}

fn collect_sysv_duplicate_sources(
    aspace: &AddrSpace,
    segments: &[RemapSegment],
    source_start: VirtAddr,
    destination: VirtAddr,
) -> AxResult<Vec<SysvDuplicateSource>> {
    let mut sources = Vec::new();
    sources
        .try_reserve_exact(segments.len())
        .map_err(|_| AxError::NoMemory)?;
    for segment in segments {
        let Some(finalizer) = segment.backend.mapping_finalizer() else {
            continue;
        };
        let Some(object_offset) = aspace.shared_backing_offset_at(segment.start) else {
            continue;
        };
        let displacement = segment.start.sub_addr(source_start);
        let destination_start = destination
            .checked_add(displacement)
            .ok_or(AxError::InvalidInput)?;
        sources.push(SysvDuplicateSource {
            finalizer_identity: finalizer.identity(),
            source_start: segment.start,
            object_offset,
            destination_start,
        });
    }
    Ok(sources)
}

fn sysv_duplicate_sources_match(aspace: &AddrSpace, sources: &[SysvDuplicateSource]) -> bool {
    sources.iter().all(|source| {
        aspace.mapping_finalizer_identity_at(source.source_start) == Some(source.finalizer_identity)
            && aspace.shared_backing_offset_at(source.source_start) == Some(source.object_offset)
    })
}

fn collect_remap_segments(
    aspace: &AddrSpace,
    start: VirtAddr,
    size: usize,
) -> AxResult<Vec<RemapSegment>> {
    let range = VirtAddrRange::try_from_start_size(start, size).ok_or(AxError::InvalidInput)?;
    let end = range.end;
    let mut cursor = start;
    let mut segments: Vec<RemapSegment> = Vec::new();
    let segment_count = aspace.areas_overlapping(range).count();
    segments
        .try_reserve_exact(segment_count)
        .map_err(|_| AxError::NoMemory)?;

    for area in aspace.areas_overlapping(range) {
        if cursor >= end {
            break;
        }
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
            && (area.lineage() != first.lineage
                || area.flags() != first.flags
                || !area.backend().compatible_with(&first.backend))
        {
            return Err(AxError::BadAddress);
        }

        segments.push(RemapSegment {
            start: cursor,
            size: seg_size,
            flags: area.flags(),
            backend: area.backend().clone(),
            lineage: area.lineage(),
        });
        cursor = seg_end;
    }

    if cursor != end {
        return Err(AxError::BadAddress);
    }

    Ok(segments)
}

fn remap_segments_match(
    aspace: &AddrSpace,
    start: VirtAddr,
    size: usize,
    expected: &[RemapSegment],
) -> bool {
    let Some(range) = VirtAddrRange::try_from_start_size(start, size) else {
        return false;
    };
    let mut cursor = start;
    let mut expected_index = 0usize;

    for area in aspace.areas_overlapping(range) {
        if cursor >= range.end {
            break;
        }
        let Some(expected_segment) = expected.get(expected_index) else {
            return false;
        };
        let segment_end = area.end().min(range.end);
        let segment_size = segment_end.sub_addr(cursor);
        if area.start() > cursor
            || expected_segment.start != cursor
            || expected_segment.size != segment_size
            || expected_segment.flags != area.flags()
            || expected_segment.lineage != area.lineage()
            || !expected_segment.backend.compatible_with(area.backend())
        {
            return false;
        }
        cursor = segment_end;
        expected_index += 1;
    }

    cursor == range.end && expected_index == expected.len()
}
fn prefix_segments(segments: &[RemapSegment], size: usize) -> AxResult<Vec<RemapSegment>> {
    let mut remaining = size;
    let mut prefix = Vec::new();
    prefix
        .try_reserve(segments.len())
        .map_err(|_| AxError::NoMemory)?;

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
            lineage: seg.lineage,
        });
        remaining -= take;
    }

    Ok(prefix)
}

fn range_is_free(aspace: &AddrSpace, start: VirtAddr, size: usize, align: usize) -> bool {
    let Some(limit) = VirtAddrRange::try_from_start_size(start, size) else {
        return false;
    };
    aspace.find_free_area(start, size, limit, align) == Some(start)
}

/// Exact-range variant for automatic growth: logical shadow-stack guards are
/// reserved just like VMAs, while fixed-address operations keep their Linux
/// VMA-only semantics.
fn range_is_free_avoiding_shadow_stack_guards(
    aspace: &AddrSpace,
    start: VirtAddr,
    size: usize,
    align: usize,
) -> bool {
    let Some(limit) = VirtAddrRange::try_from_start_size(start, size) else {
        return false;
    };
    aspace.find_free_area_avoiding_shadow_stack_guards(start, size, limit, align) == Some(start)
}

/// Revalidates the complete automatic destination reservation.  A shadow
/// stack's guard is policy-only (not a VMA), so it must be checked alongside
/// the destination again after lock-external preparation.
fn remap_destination_is_free(
    aspace: &AddrSpace,
    destination: VirtAddr,
    new_size: usize,
    page_size: usize,
    source_segments: &[RemapSegment],
) -> bool {
    let has_shadow_stack = source_segments
        .iter()
        .any(|segment| segment.flags.contains(MappingFlags::SHADOW_STACK));
    if !has_shadow_stack {
        return range_is_free(aspace, destination, new_size, page_size);
    }
    // A mixed source cannot have one coherent CET guard contract. The normal
    // collector rejects it already; retain this fail-closed check for plan
    // revalidation and future callers.
    if source_segments
        .iter()
        .any(|segment| !segment.flags.contains(MappingFlags::SHADOW_STACK))
    {
        return false;
    }
    let Some(guard_start) = destination.checked_sub(memory_addr::PAGE_SIZE_4K) else {
        return false;
    };
    let Some(total) = new_size.checked_add(memory_addr::PAGE_SIZE_4K) else {
        return false;
    };
    let Some(limit) = VirtAddrRange::try_from_start_size(guard_start, total) else {
        return false;
    };
    // Reuse the logical-guard-aware allocator here.  Plain VMA first-fit
    // would miss another SHSTK VMA whose lower guard overlaps this reservation
    // while its VMA itself begins just outside it.
    aspace.find_free_area_avoiding_shadow_stack_guards(guard_start, total, limit, page_size)
        == Some(guard_start)
}

/// Chooses an automatic mremap destination.  A moved shadow stack keeps its
/// lower guard outside the relocated VMA, so transactions continue to use the
/// returned `start` and `new_size` unchanged.
fn automatic_remap_destination(
    aspace: &AddrSpace,
    hint: VirtAddr,
    new_size: usize,
    page_size: usize,
    source_segments: &[RemapSegment],
) -> AxResult<VirtAddr> {
    let limit = VirtAddrRange::new(aspace.base(), aspace.end());
    if source_segments
        .iter()
        .any(|segment| segment.flags.contains(MappingFlags::SHADOW_STACK))
    {
        let total = new_size
            .checked_add(memory_addr::PAGE_SIZE_4K)
            .ok_or(AxError::NoMemory)?;
        let base = aspace
            .find_kernel_area(hint, total, limit, page_size)
            .ok_or(AxError::NoMemory)?;
        let start = base
            .checked_add(memory_addr::PAGE_SIZE_4K)
            .ok_or(AxError::NoMemory)?;
        if !start.is_aligned(page_size) || !aspace.contains_range(start, new_size) {
            return Err(AxError::NoMemory);
        }
        Ok(start)
    } else {
        aspace
            .find_kernel_area(hint, new_size, limit, page_size)
            .ok_or(AxError::NoMemory)
    }
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
    aspace.check_no_seal_overlap(dst, dst_size)?;
    Ok(())
}

/// Returns the destination range whose seal must be checked before mremap
/// validates the complete source extent.  Linux uses a zero-length source
/// interval for the `old_size == 0` duplication form when checking whether a
/// fixed destination overlaps its source.
fn fixed_destination_seal_probe_range(
    request: MremapRequest,
    page_size: usize,
) -> AxResult<Option<(VirtAddr, usize)>> {
    if !request.fixed {
        return Ok(None);
    }
    if !request.new_addr.is_aligned(page_size) {
        return Err(AxError::InvalidInput);
    }

    let destination = VirtAddrRange::try_from_start_size(request.new_addr, request.new_size)
        .ok_or(AxError::InvalidInput)?;
    let overlap_size = if request.old_size == 0 {
        0
    } else {
        request.old_size
    };
    let source = VirtAddrRange::try_from_start_size(request.addr, overlap_size)
        .ok_or(AxError::InvalidInput)?;
    if source.overlaps(destination) {
        return Err(AxError::InvalidInput);
    }

    Ok(Some((request.new_addr, request.new_size)))
}

fn probe_fixed_destination_seal(
    aspace: &AddrSpace,
    request: MremapRequest,
    page_size: usize,
) -> AxResult {
    let Some((destination, destination_size)) =
        fixed_destination_seal_probe_range(request, page_size)?
    else {
        return Ok(());
    };
    if !aspace.contains_range(destination, destination_size) {
        return Err(AxError::NoMemory);
    }
    aspace.check_no_seal_overlap(destination, destination_size)
}

fn source_address_matches_initial_page_size(address: VirtAddr, page_size: PageSize) -> bool {
    address.is_aligned(page_size)
}

fn check_initial_remap_source(aspace: &AddrSpace, address: VirtAddr) -> AxResult<PageSize> {
    let area = aspace.find_area(address).ok_or(AxError::BadAddress)?;
    aspace.check_vma_at_not_sealed(address)?;
    let page_size = area.backend().page_size();
    if !source_address_matches_initial_page_size(address, page_size) {
        return Err(AxError::InvalidInput);
    }
    Ok(page_size)
}

fn normalize_remap_request_geometry(
    mut request: MremapRequest,
    page_size: PageSize,
) -> AxResult<MremapRequest> {
    let page_size = page_size as usize;
    request.old_size =
        checked_align_up(request.old_size, page_size).ok_or(AxError::InvalidInput)?;
    request.new_size =
        checked_align_up(request.new_size, page_size).ok_or(AxError::InvalidInput)?;
    Ok(request)
}

struct PreparedRemapSegment {
    source_start: VirtAddr,
    destination_start: VirtAddr,
    size: usize,
    flags: MappingFlags,
    source_backend: Backend,
    destination_backend: Backend,
}

fn prepare_sysv_duplicate_admissions_for_sources(
    proc_data: &ProcessData,
    sources: &[SysvDuplicateSource],
) -> AxResult<Vec<SysvMremapDuplicateAdmission>> {
    let pid = proc_data.proc.pid();
    let namespaces = proc_data.touched_ipc_namespaces_snapshot()?;
    let mut admissions: Vec<SysvMremapDuplicateAdmission> = Vec::new();
    admissions
        .try_reserve_exact(sources.len())
        .map_err(|_| AxError::NoMemory)?;

    for source_geometry in sources {
        let source_identity = source_geometry.finalizer_identity;
        if admissions
            .iter()
            .any(|admission| admission.source_finalizer_identity() == source_identity)
        {
            continue;
        }

        let mut provenance = None;
        for namespace in &namespaces {
            let Some(record) = shm_attachment_record_by_finalizer_identity_in_namespace(
                namespace,
                pid,
                source_identity,
            ) else {
                continue;
            };
            if provenance.is_some() {
                return Err(AxError::BadState);
            }
            provenance = Some((namespace.clone(), record));
        }
        // Non-SysV mapping finalizers (for example a file listener) retain
        // their ordinary duplicate semantics and do not create IPC metadata.
        let Some((namespace, source)) = provenance else {
            continue;
        };
        let detach_base = source_geometry
            .destination_start
            .as_usize()
            .checked_sub(source_geometry.object_offset)
            .map(VirtAddr::from)
            .ok_or(AxError::BadState)?;
        let admission = prepare_sysv_mremap_duplicate_admission(
            namespace,
            pid,
            source,
            source_identity,
            detach_base,
        )?;
        admissions.push(admission);
    }
    Ok(admissions)
}

fn apply_sysv_duplicate_finalizer(
    backend: &mut Backend,
    source_identity: usize,
    admissions: &[SysvMremapDuplicateAdmission],
) {
    if let Some(admission) = admissions
        .iter()
        .find(|admission| admission.source_finalizer_identity() == source_identity)
    {
        backend.replace_mapping_finalizer(Some(admission.finalizer()));
    }
}

fn commit_sysv_duplicate_admissions(admissions: &mut [SysvMremapDuplicateAdmission]) {
    for admission in admissions {
        admission.commit();
    }
}

fn prepare_relocated_segments(
    aspace_handle: &Arc<axsync::Mutex<AddrSpace>>,
    old_start: VirtAddr,
    old_size: usize,
    new_start: VirtAddr,
    new_size: usize,
    segments: &[RemapSegment],
    preserve_backend_identity: bool,
) -> AxResult<Vec<PreparedRemapSegment>> {
    let page_size = segments
        .first()
        .ok_or(AxError::InvalidInput)?
        .backend
        .page_size() as usize;
    let geometry = RemapGeometry::new(
        LinuxPageRange::new(old_start.as_usize(), old_size, page_size)
            .map_err(|_| AxError::InvalidInput)?,
        new_start.as_usize(),
        new_size,
    )
    .map_err(|_| AxError::InvalidInput)?;
    let mut prepared = Vec::new();
    prepared
        .try_reserve_exact(segments.len())
        .map_err(|_| AxError::NoMemory)?;
    for seg in segments {
        let segment = geometry
            .segment(
                LinuxPageRange::new(seg.start.as_usize(), seg.size, page_size)
                    .map_err(|_| AxError::InvalidInput)?,
            )
            .map_err(|_| AxError::InvalidInput)?;
        let destination_start = VirtAddr::from(segment.destination().start());
        let backend_old_start = VirtAddr::from(segment.backend_old_start());
        let backend_new_start = VirtAddr::from(segment.backend_new_start());
        let relocated = if preserve_backend_identity {
            seg.backend
                .relocate(backend_old_start, backend_new_start, aspace_handle)?
        } else {
            seg.backend
                .duplicate_mapping(backend_old_start, backend_new_start, aspace_handle)?
        };
        prepared.push(PreparedRemapSegment {
            source_start: seg.start,
            destination_start,
            size: seg.size,
            flags: seg.flags,
            source_backend: seg.backend.clone(),
            destination_backend: relocated,
        });
    }
    Ok(prepared)
}

fn map_prepared_relocated_segments(
    aspace: &mut AddrSpace,
    segments: Vec<PreparedRemapSegment>,
    destination_lineage: MappingLineage,
) -> AxResult {
    // Segment VMAs are staged incrementally inside AddrSpace's transaction;
    // topology and identity publication happens once after all later work.
    // One fresh Linux-MM lineage is shared by every destination fragment; a
    // moving remap is a new mapping incarnation until EVENT_REMAP semantics
    // exist, even when the backing object's relocation identity is preserved.
    for segment in segments {
        aspace.stage_mapping_fragment(
            segment.destination_start,
            segment.size,
            segment.flags,
            false,
            segment.destination_backend,
            false,
            destination_lineage,
        )?;
        segment.source_backend.migrate_present_pages(
            segment.source_start,
            segment.destination_start,
            segment.size,
            &mut aspace.page_table_mut().cursor(),
        )?;
        aspace.relocate_swapped_entries(
            segment.source_start,
            segment.destination_start,
            segment.size,
        )?;
    }
    Ok(())
}

fn map_locked_relocated_segments(
    aspace: &mut AddrSpace,
    aspace_handle: &Arc<axsync::Mutex<AddrSpace>>,
    old_start: VirtAddr,
    old_size: usize,
    new_start: VirtAddr,
    new_size: usize,
    segments: &[RemapSegment],
    destination_lineage: MappingLineage,
    preserve_backend_identity: bool,
    sysv_admissions: &[SysvMremapDuplicateAdmission],
) -> AxResult {
    let page_size = segments
        .first()
        .ok_or(AxError::InvalidInput)?
        .backend
        .page_size() as usize;
    let geometry = RemapGeometry::new(
        LinuxPageRange::new(old_start.as_usize(), old_size, page_size)
            .map_err(|_| AxError::InvalidInput)?,
        new_start.as_usize(),
        new_size,
    )
    .map_err(|_| AxError::InvalidInput)?;

    // Fixed remaps deliberately keep every fallible backend operation inside
    // AddrSpace's transaction. Linux permits failures after MREMAP_FIXED has
    // destroyed the destination; preparing these backends before entering the
    // transaction would incorrectly preserve that destination on failure.
    for segment in segments {
        let geometry_segment = geometry
            .segment(
                LinuxPageRange::new(segment.start.as_usize(), segment.size, page_size)
                    .map_err(|_| AxError::InvalidInput)?,
            )
            .map_err(|_| AxError::InvalidInput)?;
        let destination_start = VirtAddr::from(geometry_segment.destination().start());
        let backend_old_start = VirtAddr::from(geometry_segment.backend_old_start());
        let backend_new_start = VirtAddr::from(geometry_segment.backend_new_start());
        let mut relocated = if preserve_backend_identity {
            segment
                .backend
                .relocate(backend_old_start, backend_new_start, aspace_handle)?
        } else {
            segment.backend.duplicate_mapping(
                backend_old_start,
                backend_new_start,
                aspace_handle,
            )?
        };
        if !preserve_backend_identity && let Some(finalizer) = segment.backend.mapping_finalizer() {
            apply_sysv_duplicate_finalizer(&mut relocated, finalizer.identity(), sysv_admissions);
        }
        aspace.stage_mapping_fragment(
            destination_start,
            segment.size,
            segment.flags,
            false,
            relocated,
            false,
            destination_lineage,
        )?;
        segment.backend.migrate_present_pages(
            segment.start,
            destination_start,
            segment.size,
            &mut aspace.page_table_mut().cursor(),
        )?;
        aspace.relocate_swapped_entries(segment.start, destination_start, segment.size)?;
    }
    Ok(())
}

fn check_mremap_locked_growth_limit(
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    aspace: &AddrSpace,
    grow: usize,
) -> AxResult {
    if grow == 0 || has_ipc_lock {
        return Ok(());
    }

    let limit_error = AxError::from(LinuxError::EAGAIN);
    let limit = proc_data.rlim.read()[RLIMIT_MEMLOCK].current;
    if !mremap_locked_growth_allowed(aspace.locked_bytes(), grow, limit) {
        return Err(limit_error);
    }

    Ok(())
}

fn mremap_address_space_growth(request: MremapRequest) -> usize {
    if request.old_size == 0 || request.dont_unmap {
        request.new_size
    } else {
        request.new_size.saturating_sub(request.old_size)
    }
}

fn check_mremap_address_space_limit(
    proc_data: &ProcessData,
    aspace: &AddrSpace,
    request: MremapRequest,
) -> AxResult<()> {
    if request.fixed && request.dont_unmap {
        // Linux's mremap_to() destroys a fixed destination before charging
        // the full retained-source duplicate.  The commit path below checks
        // that post-unmap total once the exact covered destination is known.
        return Ok(());
    }
    let growth = mremap_address_space_growth(request);
    if growth == 0 {
        Ok(())
    } else {
        check_rlimit_as_growth(proc_data, aspace, growth)
    }
}

fn check_mremap_commit_address_space_limit(
    proc_data: &ProcessData,
    aspace: &AddrSpace,
    request: MremapRequest,
) -> AxResult<()> {
    if request.fixed && request.dont_unmap {
        let released = aspace.mapped_bytes_in_range(request.new_addr, request.new_size)?;
        check_rlimit_as_replacement(proc_data, aspace, released, request.new_size)
    } else {
        check_mremap_address_space_limit(proc_data, aspace, request)
    }
}

fn mremap_locked_growth_allowed(
    current_locked: usize,
    additional_locked: usize,
    limit: u64,
) -> bool {
    let Ok(current_locked) = u64::try_from(current_locked) else {
        return false;
    };
    let Ok(additional_locked) = u64::try_from(additional_locked) else {
        return false;
    };
    MemlockPlan::new(
        current_locked,
        0,
        additional_locked,
        MemlockLimit::Limited(limit),
    )
    .is_ok()
}

fn accept_published_lineage_growth(result: Result<(), ExistingLineageMapError>) -> AxResult {
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.published() => Ok(()),
        Err(error) => Err(error.into_error()),
    }
}

#[derive(Clone, Copy)]
struct MremapRequest {
    addr: VirtAddr,
    old_size: usize,
    new_size: usize,
    may_move: bool,
    fixed: bool,
    dont_unmap: bool,
    new_addr: VirtAddr,
}

const fn effective_source_size(request: MremapRequest) -> usize {
    if request.old_size == 0 {
        request.new_size
    } else {
        request.old_size
    }
}

enum RemapPlan {
    Return(VirtAddr),
    Shrink {
        start: VirtAddr,
        size: usize,
    },
    Duplicate {
        source_segments: Vec<RemapSegment>,
        destination: VirtAddr,
        sysv_sources: Vec<SysvDuplicateSource>,
    },
    GrowInPlace {
        source_segments: Vec<RemapSegment>,
    },
    Move {
        source_segments: Vec<RemapSegment>,
        destination: VirtAddr,
    },
}

const fn growth_prepare_is_discardable(shared_backend: bool) -> bool {
    !shared_backend
}

impl RemapPlan {
    fn supports_unlocked_prepare(&self, request: MremapRequest) -> bool {
        if request.fixed {
            return false;
        }
        let segments = match self {
            Self::Duplicate {
                source_segments, ..
            }
            | Self::GrowInPlace { source_segments }
            | Self::Move {
                source_segments, ..
            } => source_segments,
            Self::Return(_) | Self::Shrink { .. } => return false,
        };

        // FileBackend preparation registers an eviction listener and writable
        // exclusion. Keep that path under the address-space lock until it has
        // a fully fallible prepared-registration token. Linear/Cow/Shared
        // relocation operates only on this owned backend snapshot.
        segments
            .iter()
            .all(|segment| segment.backend.shared_file_location().is_none())
            && !matches!(
                self,
                Self::GrowInPlace { source_segments }
                    | Self::Move {
                        source_segments,
                        ..
                    } if request.new_size > request.old_size
                        && !growth_prepare_is_discardable(matches!(
                            source_segments[0].backend,
                            Backend::Shared(_)
                        ))
            )
    }
}

fn build_remap_plan(
    aspace: &AddrSpace,
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    request: MremapRequest,
) -> AxResult<(MremapRequest, RemapPlan)> {
    if request.old_size == 0 {
        if !request.may_move {
            return Err(AxError::InvalidInput);
        }

        let source_page_size = check_initial_remap_source(aspace, request.addr)?;
        let request = normalize_remap_request_geometry(request, source_page_size)?;
        probe_fixed_destination_seal(aspace, request, source_page_size as usize)?;
        let source_segments = collect_remap_segments(aspace, request.addr, request.new_size)?;
        if !source_segments
            .iter()
            .all(|segment| segment.backend.is_shareable())
        {
            return Err(AxError::InvalidInput);
        }
        check_mremap_address_space_limit(proc_data, aspace, request)?;
        let page_size = source_segments[0].backend.page_size();
        let destination = if request.fixed {
            if !request.new_addr.is_aligned(page_size) {
                return Err(AxError::InvalidInput);
            }
            validate_fixed_remap_dst(
                aspace,
                request.addr,
                request.new_size,
                request.new_addr,
                request.new_size,
            )?;
            request.new_addr
        } else {
            automatic_remap_destination(
                aspace,
                request.addr,
                request.new_size,
                page_size as usize,
                &source_segments,
            )?
        };

        let duplicated_locked = aspace.locked_bytes_in_range(request.addr, request.new_size);
        if duplicated_locked != 0 {
            check_mremap_locked_growth_limit(proc_data, has_ipc_lock, aspace, duplicated_locked)?;
        }
        let sysv_sources =
            collect_sysv_duplicate_sources(aspace, &source_segments, request.addr, destination)?;
        return Ok((
            request,
            RemapPlan::Duplicate {
                source_segments,
                destination,
                sysv_sources,
            },
        ));
    }

    let source_page_size = check_initial_remap_source(aspace, request.addr)?;
    let request = normalize_remap_request_geometry(request, source_page_size)?;
    probe_fixed_destination_seal(aspace, request, source_page_size as usize)?;
    let source_segments = collect_remap_segments(aspace, request.addr, request.old_size)?;
    let page_size = source_segments[0].backend.page_size();
    if !page_size.is_aligned(request.old_size) || !page_size.is_aligned(request.new_size) {
        return Err(AxError::InvalidInput);
    }
    if request.fixed && !request.new_addr.is_aligned(page_size) {
        return Err(AxError::InvalidInput);
    }
    check_mremap_address_space_limit(proc_data, aspace, request)?;

    // DONTUNMAP is a duplication transaction, not a move transaction.  Keep
    // the source mapping published and establish a fresh destination mapping
    // with the same backend snapshot.  This is intentionally after source
    // collection: Linux still validates the original VMA before choosing an
    // automatic destination.
    if request.dont_unmap {
        let destination = if request.fixed {
            validate_fixed_remap_dst(
                aspace,
                request.addr,
                request.old_size,
                request.new_addr,
                request.new_size,
            )?;
            request.new_addr
        } else {
            automatic_remap_destination(
                aspace,
                request.addr,
                request.new_size,
                page_size as usize,
                &source_segments,
            )?
        };
        let duplicated_locked = aspace.locked_bytes_in_range(request.addr, request.old_size);
        if duplicated_locked != 0 && !request.dont_unmap {
            check_mremap_locked_growth_limit(proc_data, has_ipc_lock, aspace, duplicated_locked)?;
        }
        let sysv_sources =
            collect_sysv_duplicate_sources(aspace, &source_segments, request.addr, destination)?;
        return Ok((
            request,
            RemapPlan::Duplicate {
                source_segments,
                destination,
                sysv_sources,
            },
        ));
    }

    if !request.fixed && request.new_size == request.old_size {
        return Ok((request, RemapPlan::Return(request.addr)));
    }
    if !request.fixed && request.new_size < request.old_size {
        return Ok((
            request,
            RemapPlan::Shrink {
                start: request.addr + request.new_size,
                size: request.old_size - request.new_size,
            },
        ));
    }

    let grow = request.new_size.saturating_sub(request.old_size);
    let grow_locked = request.new_size > request.old_size
        && aspace.range_is_fully_locked(request.addr, request.old_size);
    let after = request.addr + request.old_size;
    if !request.fixed
        && request.new_size > request.old_size
        && range_is_free_avoiding_shadow_stack_guards(aspace, after, grow, page_size as usize)
    {
        if grow_locked {
            check_mremap_locked_growth_limit(proc_data, has_ipc_lock, aspace, grow)?;
        }
        return Ok((request, RemapPlan::GrowInPlace { source_segments }));
    }

    if !request.may_move {
        return Err(AxError::NoMemory);
    }

    let destination = if request.fixed {
        validate_fixed_remap_dst(
            aspace,
            request.addr,
            request.old_size,
            request.new_addr,
            request.new_size,
        )?;
        request.new_addr
    } else {
        automatic_remap_destination(
            aspace,
            request.addr,
            request.new_size,
            page_size as usize,
            &source_segments,
        )?
    };
    if grow_locked {
        check_mremap_locked_growth_limit(proc_data, has_ipc_lock, aspace, grow)?;
    }
    Ok((
        request,
        RemapPlan::Move {
            source_segments,
            destination,
        },
    ))
}

enum PreparedRemapPlan {
    Duplicate {
        source_segments: Vec<RemapSegment>,
        destination_segments: Vec<PreparedRemapSegment>,
        destination: VirtAddr,
        sysv_sources: Vec<SysvDuplicateSource>,
        sysv_admissions: Vec<SysvMremapDuplicateAdmission>,
    },
    GrowInPlace {
        source_segments: Vec<RemapSegment>,
        tail_backend: Backend,
    },
    Move {
        source_segments: Vec<RemapSegment>,
        destination_segments: Vec<PreparedRemapSegment>,
        tail_backend: Option<Backend>,
        destination: VirtAddr,
    },
}

#[derive(Clone, Copy)]
struct ExecutableGrowthTail {
    start: VirtAddr,
    size: usize,
    flags: MappingFlags,
}

fn executable_growth_tail(
    request: MremapRequest,
    destination: VirtAddr,
    primary_flags: MappingFlags,
) -> Option<ExecutableGrowthTail> {
    let size = request.new_size.checked_sub(request.old_size)?;
    (size != 0 && primary_flags.contains(MappingFlags::EXECUTE)).then_some(ExecutableGrowthTail {
        start: destination + request.old_size,
        size,
        flags: primary_flags,
    })
}

fn staged_growth_tail_flags(flags: MappingFlags) -> MappingFlags {
    flags - MappingFlags::EXECUTE
}

fn finish_executable_growth_tail_locked(
    aspace_handle: &Arc<axsync::Mutex<AddrSpace>>,
    aspace: &mut AddrSpace,
    tail: ExecutableGrowthTail,
) -> AxResult<DeferredUffdWake> {
    // A newly grown file extent can contain an already registered file-offset
    // probe even though no old virtual address existed from which to transfer
    // INT3 custody. The remap transaction publishes that extent NX; install
    // every projected probe before making the original execute permission
    // visible. Any error leaves the extent NX and therefore fail-closed.
    crate::uprobe::install_projected_exec_mapping_locked(
        aspace_handle,
        aspace,
        tail.start,
        tail.size,
    )?;
    aspace
        .prepare_protect(tail.start, tail.size, tail.flags)?
        .commit()
}

fn prepare_remap_plan(
    plan: RemapPlan,
    request: MremapRequest,
    aspace_handle: &Arc<axsync::Mutex<AddrSpace>>,
    proc_data: &ProcessData,
) -> AxResult<PreparedRemapPlan> {
    debug_assert!(!request.fixed);
    match plan {
        RemapPlan::Duplicate {
            source_segments,
            destination,
            sysv_sources,
        } => {
            let mut destination_segments = prepare_relocated_segments(
                aspace_handle,
                request.addr,
                request.new_size,
                destination,
                request.new_size,
                &source_segments,
                false,
            )?;
            let sysv_admissions =
                prepare_sysv_duplicate_admissions_for_sources(proc_data, &sysv_sources)?;
            for segment in &mut destination_segments {
                if let Some(finalizer) = segment.source_backend.mapping_finalizer() {
                    apply_sysv_duplicate_finalizer(
                        &mut segment.destination_backend,
                        finalizer.identity(),
                        &sysv_admissions,
                    );
                }
            }
            Ok(PreparedRemapPlan::Duplicate {
                source_segments,
                destination_segments,
                destination,
                sysv_sources,
                sysv_admissions,
            })
        }
        RemapPlan::GrowInPlace { source_segments } => {
            let primary = source_segments.first().ok_or(AxError::BadState)?;
            primary
                .backend
                .ensure_range_covered(request.addr, request.new_size)?;
            let tail_backend =
                primary
                    .backend
                    .relocate(request.addr, request.addr, aspace_handle)?;
            Ok(PreparedRemapPlan::GrowInPlace {
                source_segments,
                tail_backend,
            })
        }
        RemapPlan::Move {
            source_segments,
            destination,
        } => {
            let preserve_size = request.old_size.min(request.new_size);
            let moved_segments = prefix_segments(&source_segments, preserve_size)?;
            let grow = request.new_size.saturating_sub(request.old_size);
            if grow != 0 {
                source_segments[0]
                    .backend
                    .ensure_range_covered(request.addr, request.new_size)?;
            }
            let destination_segments = prepare_relocated_segments(
                aspace_handle,
                request.addr,
                request.old_size,
                destination,
                request.new_size,
                &moved_segments,
                true,
            )?;
            let tail_backend = if grow == 0 {
                None
            } else {
                Some(source_segments[0].backend.relocate(
                    request.addr,
                    destination,
                    aspace_handle,
                )?)
            };
            Ok(PreparedRemapPlan::Move {
                source_segments,
                destination_segments,
                tail_backend,
                destination,
            })
        }
        RemapPlan::Return(_) | RemapPlan::Shrink { .. } => Err(AxError::BadState),
    }
}

impl PreparedRemapPlan {
    fn revalidate(&self, aspace: &AddrSpace, request: MremapRequest) -> bool {
        debug_assert!(!request.fixed);
        match self {
            Self::Duplicate {
                source_segments,
                destination,
                sysv_sources,
                ..
            } => {
                remap_segments_match(aspace, request.addr, request.new_size, source_segments)
                    && sysv_duplicate_sources_match(aspace, sysv_sources)
                    && remap_destination_is_free(
                        aspace,
                        *destination,
                        request.new_size,
                        source_segments[0].backend.page_size() as usize,
                        source_segments,
                    )
            }
            Self::GrowInPlace {
                source_segments, ..
            } => {
                let grow = request.new_size.saturating_sub(request.old_size);
                remap_segments_match(aspace, request.addr, request.old_size, source_segments)
                    && range_is_free_avoiding_shadow_stack_guards(
                        aspace,
                        request.addr + request.old_size,
                        grow,
                        source_segments[0].backend.page_size() as usize,
                    )
            }
            Self::Move {
                source_segments,
                destination,
                ..
            } => {
                remap_segments_match(aspace, request.addr, request.old_size, source_segments)
                    && remap_destination_is_free(
                        aspace,
                        *destination,
                        request.new_size,
                        source_segments[0].backend.page_size() as usize,
                        source_segments,
                    )
            }
        }
    }
}

fn commit_prepared_remap(
    aspace: &mut AddrSpace,
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    request: MremapRequest,
    prepared: PreparedRemapPlan,
) -> LockExternalUffdOutcome<isize, AxError> {
    debug_assert!(!request.fixed);
    #[cfg(target_arch = "x86_64")]
    let cet_duplicate = matches!(&prepared, PreparedRemapPlan::Duplicate { .. });
    let mut sidecars = match &prepared {
        PreparedRemapPlan::Duplicate { destination, .. }
        | PreparedRemapPlan::Move { destination, .. }
            if *destination != request.addr =>
        {
            Some(aspace.prepare_remap_madvise_sidecars(
                request.addr,
                effective_source_size(request),
                *destination,
                request.new_size,
                request.dont_unmap,
            ))
        }
        _ => None,
    };
    let mut wake = DeferredUffdWake::empty();
    let outcome = (|| {
        check_mremap_address_space_limit(proc_data, aspace, request)?;
        #[cfg(target_arch = "x86_64")]
        aspace.prepare_cet_default_shadow_stacks_for_mremap(
            request.addr,
            effective_source_size(request),
            cet_duplicate,
        )?;
        match prepared {
            PreparedRemapPlan::Duplicate {
                source_segments: _,
                destination_segments,
                destination,
                mut sysv_admissions,
                ..
            } => {
                let duplicated_locked =
                    aspace.locked_bytes_in_range(request.addr, request.new_size);
                if duplicated_locked != 0 && !request.dont_unmap {
                    check_mremap_locked_growth_limit(
                        proc_data,
                        has_ipc_lock,
                        aspace,
                        duplicated_locked,
                    )?;
                }
                let staged_fragments = destination_segments.len();
                let duplicate = aspace.duplicate_mapping_into_empty_transaction(
                    request.addr,
                    request.new_size,
                    destination,
                    request.new_size,
                    staged_fragments,
                    move |aspace, destination_lineage| {
                        map_prepared_relocated_segments(
                            aspace,
                            destination_segments,
                            destination_lineage,
                        )
                    },
                );
                let (duplicate, transaction_wake) = duplicate.into_parts();
                wake.merge(transaction_wake);
                duplicate?;
                commit_sysv_duplicate_admissions(&mut sysv_admissions);
                if request.dont_unmap {
                    // Linux transfers VM_LOCKED/VM_LOCKONFAULT ownership to
                    // the new VMA and clears it from the retained source, so
                    // DONTUNMAP does not double RLIMIT_MEMLOCK accounting.
                    aspace.clear_locked_range(request.addr, request.new_size);
                }
                Ok(destination.as_usize() as isize)
            }
            PreparedRemapPlan::GrowInPlace {
                source_segments,
                tail_backend,
            } => {
                let grow = request.new_size - request.old_size;
                let primary = &source_segments[0];
                let grow_locked = aspace.range_is_fully_locked(request.addr, request.old_size);
                if grow_locked {
                    check_mremap_locked_growth_limit(proc_data, has_ipc_lock, aspace, grow)?;
                }
                accept_published_lineage_growth(aspace.extend_mapping_tail_with_existing_lineage(
                    request.addr + request.old_size,
                    grow,
                    staged_growth_tail_flags(primary.flags),
                    grow_locked,
                    tail_backend,
                    grow_locked,
                    primary.lineage,
                ))?;
                Ok(request.addr.as_usize() as isize)
            }
            PreparedRemapPlan::Move {
                source_segments,
                destination_segments,
                tail_backend,
                destination,
            } => {
                let grow = request.new_size.saturating_sub(request.old_size);
                let grow_locked =
                    grow != 0 && aspace.range_is_fully_locked(request.addr, request.old_size);
                if grow_locked {
                    check_mremap_locked_growth_limit(proc_data, has_ipc_lock, aspace, grow)?;
                }
                let primary_flags = source_segments[0].flags;
                let staged_fragments = destination_segments
                    .len()
                    .checked_add(usize::from(tail_backend.is_some()))
                    .ok_or(AxError::NoMemory)?;
                let moved = aspace.move_mapping_into_empty_transaction(
                    request.addr,
                    request.old_size,
                    destination,
                    request.new_size,
                    staged_fragments,
                    move |aspace, destination_lineage| {
                        map_prepared_relocated_segments(
                            aspace,
                            destination_segments,
                            destination_lineage,
                        )?;
                        if let Some(tail_backend) = tail_backend {
                            aspace.stage_mapping_fragment(
                                destination + request.old_size,
                                grow,
                                staged_growth_tail_flags(primary_flags),
                                grow_locked,
                                tail_backend,
                                false,
                                destination_lineage,
                            )?;
                        }
                        Ok(())
                    },
                );
                let (moved, transaction_wake) = moved.into_parts();
                wake.merge(transaction_wake);
                moved?;

                proc_data.clear_mempolicy_range(request.addr.as_usize(), request.old_size);
                proc_data.clear_mempolicy_range(destination.as_usize(), request.new_size);
                Ok(destination.as_usize() as isize)
            }
        }
    })();
    let outcome = outcome.map(|destination| {
        let destination = VirtAddr::from(destination as usize);
        if destination != request.addr {
            // VMA/PTE publication is already committed while this same mm
            // mutex is held. Move fault-policy sidecars before exposing the
            // result to another CLONE_VM task; DONTUNMAP retains the source.
            aspace.commit_prepared_madvise_sidecars(
                sidecars.take().expect("prepared remap sidecars"),
            );
        }
        // Owner records track automatic cleanup only.  They must never
        // authorize an active SSP or cause a remote task to disable CET.
        #[cfg(target_arch = "x86_64")]
        aspace.rebase_cet_default_shadow_stacks_after_mremap(
            request.addr,
            effective_source_size(request),
            request.new_size,
            destination,
            cet_duplicate,
        );
        destination.as_usize() as isize
    });
    LockExternalUffdOutcome::new(outcome, wake)
}

fn commit_locked_remap(
    aspace: &mut AddrSpace,
    aspace_handle: &Arc<axsync::Mutex<AddrSpace>>,
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    request: MremapRequest,
    plan: RemapPlan,
    sysv_admissions: &mut [SysvMremapDuplicateAdmission],
) -> LockExternalUffdOutcome<isize, AxError> {
    #[cfg(target_arch = "x86_64")]
    let cet_duplicate = matches!(&plan, RemapPlan::Duplicate { .. });
    let mut sidecars = match &plan {
        RemapPlan::Duplicate { destination, .. } | RemapPlan::Move { destination, .. }
            if *destination != request.addr =>
        {
            Some(aspace.prepare_remap_madvise_sidecars(
                request.addr,
                effective_source_size(request),
                *destination,
                request.new_size,
                request.dont_unmap,
            ))
        }
        _ => None,
    };
    let mut wake = DeferredUffdWake::empty();
    let outcome = (|| {
        if request.fixed && request.dont_unmap {
            // Linux mremap_to() performs this destination munmap before its
            // full-source may_expand_vm() check.  Keep that intentionally
            // destructive ordering: ENOMEM or a later copy failure leaves
            // the old destination absent while the source remains intact.
            wake.merge(aspace.unmap(request.new_addr, request.new_size)?);
            proc_data.clear_mempolicy_range(request.new_addr.as_usize(), request.new_size);
        }
        check_mremap_commit_address_space_limit(proc_data, aspace, request)?;
        #[cfg(target_arch = "x86_64")]
        aspace.prepare_cet_default_shadow_stacks_for_mremap(
            request.addr,
            effective_source_size(request),
            cet_duplicate,
        )?;
        match plan {
            RemapPlan::Return(address) => Ok(address.as_usize() as isize),
            RemapPlan::Shrink { start, size } => {
                wake.merge(aspace.unmap(start, size)?);
                proc_data.clear_mempolicy_range(start.as_usize(), size);
                Ok(request.addr.as_usize() as isize)
            }
            RemapPlan::Duplicate {
                source_segments,
                destination,
                ..
            } => {
                let duplicated_locked =
                    aspace.locked_bytes_in_range(request.addr, request.new_size);
                if duplicated_locked != 0 && !request.dont_unmap {
                    check_mremap_locked_growth_limit(
                        proc_data,
                        has_ipc_lock,
                        aspace,
                        duplicated_locked,
                    )?;
                }
                let staged_fragments = source_segments.len();
                let duplicate = if request.fixed && !request.dont_unmap {
                    aspace
                        .replace_and_duplicate_mapping_transaction(
                            request.addr,
                            request.new_size,
                            destination,
                            request.new_size,
                            staged_fragments,
                            |aspace, destination_lineage| {
                                map_locked_relocated_segments(
                                    aspace,
                                    aspace_handle,
                                    request.addr,
                                    request.new_size,
                                    destination,
                                    request.new_size,
                                    &source_segments,
                                    destination_lineage,
                                    false,
                                    sysv_admissions,
                                )
                            },
                        )
                        .map_err(|error| {
                            if error.mapping_changed() {
                                proc_data.clear_mempolicy_range(
                                    destination.as_usize(),
                                    request.new_size,
                                );
                            }
                            error.into_error()
                        })
                } else {
                    aspace.duplicate_mapping_into_empty_transaction(
                        request.addr,
                        request.new_size,
                        destination,
                        request.new_size,
                        staged_fragments,
                        |aspace, destination_lineage| {
                            map_locked_relocated_segments(
                                aspace,
                                aspace_handle,
                                request.addr,
                                request.new_size,
                                destination,
                                request.new_size,
                                &source_segments,
                                destination_lineage,
                                false,
                                sysv_admissions,
                            )
                        },
                    )
                };
                let (duplicate, transaction_wake) = duplicate.into_parts();
                wake.merge(transaction_wake);
                duplicate?;
                commit_sysv_duplicate_admissions(sysv_admissions);
                if request.dont_unmap {
                    aspace.clear_locked_range(request.addr, request.new_size);
                }
                if request.fixed {
                    proc_data.clear_mempolicy_range(destination.as_usize(), request.new_size);
                }
                Ok(destination.as_usize() as isize)
            }
            RemapPlan::GrowInPlace { source_segments } => {
                let grow = request.new_size - request.old_size;
                let primary = &source_segments[0];
                let grow_locked = aspace.range_is_fully_locked(request.addr, request.old_size);
                if grow_locked {
                    check_mremap_locked_growth_limit(proc_data, has_ipc_lock, aspace, grow)?;
                }
                primary
                    .backend
                    .ensure_range_covered(request.addr, request.new_size)?;
                let tail_backend =
                    primary
                        .backend
                        .relocate(request.addr, request.addr, aspace_handle)?;
                accept_published_lineage_growth(aspace.extend_mapping_tail_with_existing_lineage(
                    request.addr + request.old_size,
                    grow,
                    staged_growth_tail_flags(primary.flags),
                    grow_locked,
                    tail_backend,
                    grow_locked,
                    primary.lineage,
                ))?;
                Ok(request.addr.as_usize() as isize)
            }
            RemapPlan::Move {
                source_segments,
                destination,
            } => {
                let preserve_size = request.old_size.min(request.new_size);
                let moved_segments = prefix_segments(&source_segments, preserve_size)?;
                let grow = request.new_size.saturating_sub(request.old_size);
                let primary = &source_segments[0];
                let grow_locked =
                    grow != 0 && aspace.range_is_fully_locked(request.addr, request.old_size);
                if grow_locked {
                    check_mremap_locked_growth_limit(proc_data, has_ipc_lock, aspace, grow)?;
                }
                if grow != 0 {
                    primary
                        .backend
                        .ensure_range_covered(request.addr, request.new_size)?;
                }
                let staged_fragments = moved_segments
                    .len()
                    .checked_add(usize::from(grow != 0))
                    .ok_or(AxError::NoMemory)?;
                let moved = if request.fixed {
                    aspace
                        .replace_and_move_mapping_transaction(
                            request.addr,
                            request.old_size,
                            destination,
                            request.new_size,
                            staged_fragments,
                            |aspace, destination_lineage| {
                                map_locked_relocated_segments(
                                    aspace,
                                    aspace_handle,
                                    request.addr,
                                    request.old_size,
                                    destination,
                                    request.new_size,
                                    &moved_segments,
                                    destination_lineage,
                                    true,
                                    &[],
                                )?;
                                if grow != 0 {
                                    let tail_backend = primary.backend.relocate(
                                        request.addr,
                                        destination,
                                        aspace_handle,
                                    )?;
                                    aspace.stage_mapping_fragment(
                                        destination + request.old_size,
                                        grow,
                                        staged_growth_tail_flags(primary.flags),
                                        grow_locked,
                                        tail_backend,
                                        false,
                                        destination_lineage,
                                    )?;
                                }
                                Ok(())
                            },
                        )
                        .map_err(|error| {
                            if error.mapping_changed() {
                                proc_data.clear_mempolicy_range(
                                    destination.as_usize(),
                                    request.new_size,
                                );
                            }
                            error.into_error()
                        })
                } else {
                    aspace.move_mapping_into_empty_transaction(
                        request.addr,
                        request.old_size,
                        destination,
                        request.new_size,
                        staged_fragments,
                        |aspace, destination_lineage| {
                            map_locked_relocated_segments(
                                aspace,
                                aspace_handle,
                                request.addr,
                                request.old_size,
                                destination,
                                request.new_size,
                                &moved_segments,
                                destination_lineage,
                                true,
                                &[],
                            )?;
                            if grow != 0 {
                                let tail_backend = primary.backend.relocate(
                                    request.addr,
                                    destination,
                                    aspace_handle,
                                )?;
                                aspace.stage_mapping_fragment(
                                    destination + request.old_size,
                                    grow,
                                    staged_growth_tail_flags(primary.flags),
                                    grow_locked,
                                    tail_backend,
                                    false,
                                    destination_lineage,
                                )?;
                            }
                            Ok(())
                        },
                    )
                };
                let (moved, transaction_wake) = moved.into_parts();
                wake.merge(transaction_wake);
                moved?;
                proc_data.clear_mempolicy_range(request.addr.as_usize(), request.old_size);
                proc_data.clear_mempolicy_range(destination.as_usize(), request.new_size);
                Ok(destination.as_usize() as isize)
            }
        }
    })();
    let outcome = outcome.map(|destination| {
        let destination = VirtAddr::from(destination as usize);
        if destination != request.addr {
            aspace.commit_prepared_madvise_sidecars(
                sidecars.take().expect("prepared remap sidecars"),
            );
        }
        #[cfg(target_arch = "x86_64")]
        if request.fixed {
            // MREMAP_FIXED destroys the old destination through an internal
            // remap transaction rather than AddrSpace::unmap. Remove those
            // owners before publishing any relocated/duplicated source
            // extents at the same addresses.
            aspace.remove_cet_default_shadow_stack_extents_for_unmap(destination, request.new_size);
        }
        #[cfg(target_arch = "x86_64")]
        aspace.rebase_cet_default_shadow_stacks_after_mremap(
            request.addr,
            effective_source_size(request),
            request.new_size,
            destination,
            cet_duplicate,
        );
        destination.as_usize() as isize
    });
    LockExternalUffdOutcome::new(outcome, wake)
}

enum OptimisticRemapOutcome {
    Complete(isize),
    Retry,
    LockedFallback,
}

fn try_optimistic_mremap(
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    request: MremapRequest,
) -> AxResult<OptimisticRemapOutcome> {
    let aspace_handle = proc_data.aspace();
    let source_size = if request.old_size == 0 {
        request.new_size
    } else {
        request.old_size
    };
    {
        let aspace = super::lock_mm_diagnosed!(aspace_handle, MremapSerialized);
        aspace.reject_special_mapping_mutation(request.addr, source_size)?;
        if request.fixed {
            aspace.reject_special_mapping_overlap(request.new_addr, request.new_size)?;
        }
        crate::uprobe::invalidate_xol_range_locked(&aspace, request.addr, source_size);
        if request.fixed {
            crate::uprobe::invalidate_xol_range_locked(&aspace, request.new_addr, request.new_size);
        }
    }
    ensure_4k_granularity_across_aliases(&aspace_handle, request.addr, source_size)?;
    if request.fixed {
        // MREMAP_FIXED replaces the destination through AddrSpace's ordinary
        // single-mm unmap transaction.  Demote a shared compound destination
        // across every alias before entering that path.
        ensure_4k_granularity_across_aliases(&aspace_handle, request.new_addr, request.new_size)?;
    }
    let mut aspace = super::lock_mm_diagnosed!(aspace_handle, MremapOptimisticPlan);
    if !proc_data.image_matches(&aspace_handle) {
        return Ok(OptimisticRemapOutcome::Retry);
    }
    // Revalidate after the lock-external alias-demotion phase. A concurrent
    // #BP may have allocated a fresh XOL special VMA in either range.
    aspace.reject_special_mapping_mutation(request.addr, source_size)?;
    if request.fixed {
        aspace.reject_special_mapping_overlap(request.new_addr, request.new_size)?;
    }
    // A remap may relocate, duplicate, shrink, or replace either range. Kill
    // any XOL identity before planner-side 4KiB demotion or VMA mutation.
    crate::uprobe::invalidate_xol_range_locked(&aspace, request.addr, source_size);
    if request.fixed {
        crate::uprobe::invalidate_xol_range_locked(&aspace, request.new_addr, request.new_size);
    }
    // A source range may start or end inside a promoted private-COW PMD.
    // mremap's move/duplicate machinery is VMA/page granular, so restore PTE
    // geometry before collecting source fragments or deriving backend offsets.
    aspace.ensure_4k_granularity(request.addr, source_size)?;
    let (request, plan) = build_remap_plan(&aspace, proc_data, has_ipc_lock, request)?;
    match plan {
        RemapPlan::Return(address) => {
            return Ok(OptimisticRemapOutcome::Complete(address.as_usize() as isize));
        }
        RemapPlan::Shrink { start, size } => {
            let wake = aspace.unmap(start, size)?;
            proc_data.clear_mempolicy_range(start.as_usize(), size);
            #[cfg(target_arch = "x86_64")]
            aspace.rebase_cet_default_shadow_stacks_after_mremap(
                request.addr,
                request.old_size,
                request.new_size,
                request.addr,
                false,
            );
            let reconcile = crate::uprobe::reconcile_mm_locked_gated(&aspace_handle, &mut aspace);
            drop(aspace);
            wake.finish();
            reconcile?;
            return Ok(OptimisticRemapOutcome::Complete(
                request.addr.as_usize() as isize
            ));
        }
        _ if !plan.supports_unlocked_prepare(request) => {
            return Ok(OptimisticRemapOutcome::LockedFallback);
        }
        _ => {}
    }
    drop(aspace);

    let prepared = prepare_remap_plan(plan, request, &aspace_handle, proc_data)?;
    let mut aspace = super::lock_mm_diagnosed!(aspace_handle, MremapOptimisticCommit);
    if !proc_data.image_matches(&aspace_handle) || !prepared.revalidate(&aspace, request) {
        return Ok(OptimisticRemapOutcome::Retry);
    }
    // `prepare_remap_plan` is lock-external. Recheck special VMA identity at
    // the final commit edge so a concurrent uprobe cannot be relocated.
    aspace.reject_special_mapping_mutation(request.addr, source_size)?;
    if request.fixed {
        aspace.reject_special_mapping_overlap(request.new_addr, request.new_size)?;
    }
    let transfer = match &prepared {
        PreparedRemapPlan::Duplicate { destination, .. } => Some((
            *destination,
            true,
            crate::uprobe::prepare_remap_topology_transfer_locked(
                &aspace,
                request.addr,
                source_size,
                *destination,
                request.new_size,
            )?,
        )),
        PreparedRemapPlan::Move { destination, .. } => Some((
            *destination,
            false,
            crate::uprobe::prepare_remap_topology_transfer_locked(
                &aspace,
                request.addr,
                source_size,
                *destination,
                request.new_size,
            )?,
        )),
        PreparedRemapPlan::GrowInPlace { .. } => None,
    };
    let executable_tail = match &prepared {
        PreparedRemapPlan::GrowInPlace {
            source_segments, ..
        } => executable_growth_tail(request, request.addr, source_segments[0].flags),
        PreparedRemapPlan::Move {
            source_segments,
            destination,
            ..
        } => executable_growth_tail(request, *destination, source_segments[0].flags),
        PreparedRemapPlan::Duplicate { .. } => None,
    };
    let committed = commit_prepared_remap(&mut aspace, proc_data, has_ipc_lock, request, prepared);
    let (result, mut wake) = committed.into_parts();
    let commit_succeeded = result.is_ok();
    if let Some((destination, duplicate, transfer)) = transfer {
        crate::uprobe::commit_remap_topology_transfer_locked(
            &mut aspace,
            transfer,
            request.addr,
            source_size,
            destination,
            request.new_size,
            duplicate,
            commit_succeeded,
        );
    }
    let tail_result = if commit_succeeded && let Some(tail) = executable_tail {
        finish_executable_growth_tail_locked(&aspace_handle, &mut aspace, tail)
            .map(|tail_wake| wake.merge(tail_wake))
    } else {
        Ok(())
    };
    let reconcile = crate::uprobe::reconcile_mm_locked_gated(&aspace_handle, &mut aspace);
    drop(aspace);
    wake.finish();
    // A destructive commit error must never hide loss of probe ownership.
    // Reconciliation has already run under the mm lock; report its failure
    // first so the caller cannot mistake an un-reconciled topology for an
    // ordinary remap error.
    if let Err(error) = reconcile {
        return Err(error);
    }
    let result = result?;
    tail_result?;
    Ok(OptimisticRemapOutcome::Complete(result))
}

fn run_locked_mremap(
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    request: MremapRequest,
) -> AxResult<isize> {
    loop {
        let aspace_handle = proc_data.aspace();
        let source_size = if request.old_size == 0 {
            request.new_size
        } else {
            request.old_size
        };
        {
            let aspace = super::lock_mm_diagnosed!(aspace_handle, MremapSerialized);
            aspace.reject_special_mapping_mutation(request.addr, source_size)?;
            if request.fixed {
                aspace.reject_special_mapping_overlap(request.new_addr, request.new_size)?;
            }
            crate::uprobe::invalidate_xol_range_locked(&aspace, request.addr, source_size);
            if request.fixed {
                crate::uprobe::invalidate_xol_range_locked(
                    &aspace,
                    request.new_addr,
                    request.new_size,
                );
            }
        }
        ensure_4k_granularity_across_aliases(&aspace_handle, request.addr, source_size)?;
        if request.fixed {
            ensure_4k_granularity_across_aliases(
                &aspace_handle,
                request.new_addr,
                request.new_size,
            )?;
        }
        let mut aspace = super::lock_mm_diagnosed!(aspace_handle, MremapSerialized);
        if !proc_data.image_matches(&aspace_handle) {
            continue;
        }
        // The preceding alias-demotion helper released this mutex; reject a
        // newly installed XOL VMA while this final destructive transaction is
        // still exclusively owned.
        aspace.reject_special_mapping_mutation(request.addr, source_size)?;
        if request.fixed {
            aspace.reject_special_mapping_overlap(request.new_addr, request.new_size)?;
        }
        crate::uprobe::invalidate_xol_range_locked(&aspace, request.addr, source_size);
        if request.fixed {
            crate::uprobe::invalidate_xol_range_locked(&aspace, request.new_addr, request.new_size);
        }
        // The outer planner probe is intentionally lock-external so it can
        // sleep.  Recheck after acquiring the mutation lock: a pageout fence
        // may have been published in that gap, and both 4KiB demotion and
        // the later move/copy can publish file aliases.
        let retry = aspace
            .file_eviction_retry_for_range(request.addr, source_size)
            .or_else(|| {
                request
                    .fixed
                    .then(|| {
                        aspace.file_eviction_retry_for_range(request.new_addr, request.new_size)
                    })
                    .flatten()
            });
        if let Some(retry) = retry {
            drop(aspace);
            retry.wait()?;
            continue;
        }
        // Keep the locked and optimistic paths identical: partial move and
        // MREMAP_DONTUNMAP duplication must never slice one private huge leaf.
        aspace.ensure_4k_granularity(request.addr, source_size)?;
        let (request, plan) = build_remap_plan(&aspace, proc_data, has_ipc_lock, request)?;
        match &plan {
            RemapPlan::Return(address) => return Ok(address.as_usize() as isize),
            RemapPlan::Shrink { start, size } => {
                let wake = aspace.unmap(*start, *size)?;
                proc_data.clear_mempolicy_range(start.as_usize(), *size);
                #[cfg(target_arch = "x86_64")]
                aspace.rebase_cet_default_shadow_stacks_after_mremap(
                    request.addr,
                    request.old_size,
                    request.new_size,
                    request.addr,
                    false,
                );
                let reconcile =
                    crate::uprobe::reconcile_mm_locked_gated(&aspace_handle, &mut aspace);
                drop(aspace);
                wake.finish();
                reconcile?;
                return Ok(request.addr.as_usize() as isize);
            }
            _ => {}
        }
        let mut sysv_admissions = if let RemapPlan::Duplicate {
            source_segments,
            sysv_sources,
            destination,
        } = &plan
        {
            let sources = sysv_sources.clone();
            drop(aspace);
            let admissions = prepare_sysv_duplicate_admissions_for_sources(proc_data, &sources)?;
            aspace = super::lock_mm_diagnosed!(aspace_handle, MremapSysvDuplicateCommit);
            if !proc_data.image_matches(&aspace_handle)
                || !remap_segments_match(&aspace, request.addr, source_size, source_segments)
                || !sysv_duplicate_sources_match(&aspace, &sources)
                || (!request.fixed
                    && !remap_destination_is_free(
                        &aspace,
                        *destination,
                        request.new_size,
                        source_segments[0].backend.page_size() as usize,
                        source_segments,
                    ))
            {
                continue;
            }
            aspace.reject_special_mapping_mutation(request.addr, source_size)?;
            if request.fixed {
                aspace.reject_special_mapping_overlap(request.new_addr, request.new_size)?;
            }
            admissions
        } else {
            Vec::new()
        };
        let transfer = match &plan {
            RemapPlan::Duplicate { destination, .. } => Some((
                *destination,
                true,
                crate::uprobe::prepare_remap_topology_transfer_locked(
                    &aspace,
                    request.addr,
                    source_size,
                    *destination,
                    request.new_size,
                )?,
            )),
            RemapPlan::Move { destination, .. } => Some((
                *destination,
                false,
                crate::uprobe::prepare_remap_topology_transfer_locked(
                    &aspace,
                    request.addr,
                    source_size,
                    *destination,
                    request.new_size,
                )?,
            )),
            RemapPlan::Return(_) | RemapPlan::Shrink { .. } | RemapPlan::GrowInPlace { .. } => None,
        };
        let executable_tail = match &plan {
            RemapPlan::GrowInPlace { source_segments } => {
                executable_growth_tail(request, request.addr, source_segments[0].flags)
            }
            RemapPlan::Move {
                source_segments,
                destination,
            } => executable_growth_tail(request, *destination, source_segments[0].flags),
            RemapPlan::Return(_) | RemapPlan::Shrink { .. } | RemapPlan::Duplicate { .. } => None,
        };
        let committed = commit_locked_remap(
            &mut aspace,
            &aspace_handle,
            proc_data,
            has_ipc_lock,
            request,
            plan,
            &mut sysv_admissions,
        );
        let (result, mut wake) = committed.into_parts();
        let commit_succeeded = result.is_ok();
        if let Some((destination, duplicate, transfer)) = transfer {
            crate::uprobe::commit_remap_topology_transfer_locked(
                &mut aspace,
                transfer,
                request.addr,
                source_size,
                destination,
                request.new_size,
                duplicate,
                commit_succeeded,
            );
        }
        let tail_result = if commit_succeeded && let Some(tail) = executable_tail {
            finish_executable_growth_tail_locked(&aspace_handle, &mut aspace, tail)
                .map(|tail_wake| wake.merge(tail_wake))
        } else {
            Ok(())
        };
        let reconcile = crate::uprobe::reconcile_mm_locked_gated(&aspace_handle, &mut aspace);
        drop(aspace);
        wake.finish();
        if let Err(error) = reconcile {
            return Err(error);
        }
        let result = result?;
        tail_result?;
        return Ok(result);
    }
}

pub(crate) fn remap_user_mapping(
    proc_data: &ProcessData,
    has_ipc_lock: bool,
    addr: VirtAddr,
    old_size: usize,
    new_size: usize,
    may_move: bool,
    fixed: bool,
    dont_unmap: bool,
    new_addr: VirtAddr,
) -> AxResult<isize> {
    const OPTIMISTIC_RETRIES: usize = 2;

    let request = MremapRequest {
        addr,
        old_size,
        new_size,
        may_move,
        fixed,
        dont_unmap,
        new_addr,
    };
    // Moving/copying a file VMA can publish aliases in both the source and
    // fixed destination ranges.  Wait outside the mm mutex and restart the
    // complete mremap planner so offsets, VMAs and lock state are all freshly
    // derived after an eviction commit or abort.
    loop {
        let aspace_handle = proc_data.aspace();
        let retry = {
            let aspace = aspace_handle.lock();
            aspace
                .file_eviction_retry_for_range(request.addr, request.old_size.max(request.new_size))
                .or_else(|| {
                    request
                        .fixed
                        .then(|| {
                            aspace.file_eviction_retry_for_range(request.new_addr, request.new_size)
                        })
                        .flatten()
                })
        };
        let Some(retry) = retry else {
            break;
        };
        retry.wait()?;
    }
    // Serialize every lock-external preparation and final VMA commit with
    // global uprobe registration. Successful paths reconcile while still
    // holding the mm lock, before another thread can execute a moved INT3.
    let _uprobe_topology = crate::uprobe::registration_topology_gate();
    if fixed {
        return run_locked_mremap(proc_data, has_ipc_lock, request);
    }
    for _ in 0..OPTIMISTIC_RETRIES {
        match try_optimistic_mremap(proc_data, has_ipc_lock, request)? {
            OptimisticRemapOutcome::Complete(result) => return Ok(result),
            OptimisticRemapOutcome::Retry => continue,
            OptimisticRemapOutcome::LockedFallback => break,
        }
    }

    // Contended snapshots and backends with publication side effects retain
    // the serialized path. Internal optimistic retries never escape as errno.
    run_locked_mremap(proc_data, has_ipc_lock, request)
}

#[cfg(test)]
mod tests {
    use axhal::paging::{MappingFlags, PageSize};
    use memory_addr::PAGE_SIZE_4K;

    use super::*;

    #[test]
    fn remap_fragments_share_one_backend_relocation_pair() {
        let old_start = VirtAddr::from(0x8000);
        let new_start = VirtAddr::from(0x1000);
        let geometry = RemapGeometry::new(
            LinuxPageRange::new(old_start.as_usize(), 0x4000, PAGE_SIZE_4K).unwrap(),
            new_start.as_usize(),
            0x4000,
        )
        .unwrap();
        let first = geometry
            .segment(LinuxPageRange::new(old_start.as_usize(), 0x2000, PAGE_SIZE_4K).unwrap())
            .unwrap();
        let second = geometry
            .segment(
                LinuxPageRange::new((old_start + 0x2000).as_usize(), 0x2000, PAGE_SIZE_4K).unwrap(),
            )
            .unwrap();

        assert_eq!(first.destination().start(), new_start.as_usize());
        assert_eq!(
            second.destination().start(),
            (new_start + 0x2000).as_usize()
        );
        assert_eq!(first.backend_old_start(), old_start.as_usize());
        assert_eq!(second.backend_old_start(), old_start.as_usize());
        assert_eq!(first.backend_new_start(), new_start.as_usize());
        assert_eq!(second.backend_new_start(), new_start.as_usize());
    }

    #[test]
    fn mremap_locked_growth_uses_linux_pre_replacement_accounting() {
        let page = PAGE_SIZE_4K;
        assert!(mremap_locked_growth_allowed(
            2 * page,
            page,
            (3 * page) as u64
        ));
        assert!(!mremap_locked_growth_allowed(
            2 * page,
            page,
            (2 * page) as u64
        ));
        assert!(!mremap_locked_growth_allowed(usize::MAX, 1, u64::MAX));
    }

    #[test]
    fn unlocked_prepare_excludes_fixed_and_published_shared_growth() {
        let grow = RemapPlan::GrowInPlace {
            source_segments: Vec::new(),
        };
        let request = MremapRequest {
            addr: VirtAddr::from(0x4000),
            old_size: PAGE_SIZE_4K,
            new_size: 2 * PAGE_SIZE_4K,
            may_move: true,
            fixed: true,
            dont_unmap: false,
            new_addr: VirtAddr::from(0),
        };
        assert!(!grow.supports_unlocked_prepare(request));
        assert!(growth_prepare_is_discardable(false));
        assert!(!growth_prepare_is_discardable(true));
    }

    #[test]
    fn sealed_fixed_destination_precedes_invalid_source_extent() {
        let source = VirtAddr::from(0x2000);
        let destination = VirtAddr::from(0x6000);
        let mut aspace = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x8000).unwrap();
        aspace
            .map(
                source,
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                Backend::new_alloc(source, PageSize::Size4K),
            )
            .unwrap();
        aspace
            .map(
                destination,
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                Backend::new_alloc(destination, PageSize::Size4K),
            )
            .unwrap();
        aspace.seal(destination, PAGE_SIZE_4K).unwrap();

        let request = MremapRequest {
            addr: source,
            old_size: PAGE_SIZE_4K * 2,
            new_size: PAGE_SIZE_4K,
            may_move: true,
            fixed: true,
            dont_unmap: false,
            new_addr: destination,
        };

        // The initial lookup succeeds, but the requested extent crosses the
        // source VMA boundary. A sealed fixed destination wins over that
        // later EFAULT.
        assert_eq!(
            check_initial_remap_source(&aspace, source),
            Ok(PageSize::Size4K)
        );
        assert_eq!(
            probe_fixed_destination_seal(&aspace, request, PAGE_SIZE_4K),
            Err(AxError::OperationNotPermitted)
        );
        assert!(matches!(
            collect_remap_segments(&aspace, source, request.old_size),
            Err(AxError::BadAddress)
        ));
    }

    #[test]
    fn fixed_destination_probe_uses_zero_length_old_size_for_overlap() {
        let request = MremapRequest {
            addr: VirtAddr::from(0x4000),
            old_size: 0,
            new_size: PAGE_SIZE_4K,
            may_move: true,
            fixed: true,
            dont_unmap: false,
            new_addr: VirtAddr::from(0x4000),
        };

        assert_eq!(
            fixed_destination_seal_probe_range(request, PAGE_SIZE_4K).unwrap(),
            Some((request.new_addr, request.new_size))
        );
    }

    #[test]
    fn fixed_destination_probe_honors_initial_huge_page_alignment() {
        let request = MremapRequest {
            addr: VirtAddr::from(0x20_0000),
            old_size: PageSize::Size2M as usize,
            new_size: PAGE_SIZE_4K,
            may_move: true,
            fixed: true,
            dont_unmap: false,
            new_addr: VirtAddr::from(0x6000),
        };

        assert_eq!(
            fixed_destination_seal_probe_range(request, PageSize::Size2M as usize),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn huge_initial_vma_requires_huge_aligned_source_address() {
        assert!(!source_address_matches_initial_page_size(
            VirtAddr::from(0x20_1000),
            PageSize::Size2M
        ));
        assert!(source_address_matches_initial_page_size(
            VirtAddr::from(0x20_0000),
            PageSize::Size2M
        ));
    }

    #[test]
    fn huge_initial_vma_normalizes_fixed_destination_geometry_before_seal_probe() {
        let request = MremapRequest {
            addr: VirtAddr::from(0x20_0000),
            old_size: PageSize::Size2M as usize,
            new_size: PAGE_SIZE_4K,
            may_move: true,
            fixed: true,
            dont_unmap: false,
            new_addr: VirtAddr::from(0x40_0000),
        };
        let request = normalize_remap_request_geometry(request, PageSize::Size2M).unwrap();

        assert_eq!(request.new_size, PageSize::Size2M as usize);
        assert_eq!(
            fixed_destination_seal_probe_range(request, PageSize::Size2M as usize).unwrap(),
            Some((request.new_addr, PageSize::Size2M as usize))
        );
    }

    #[test]
    fn huge_initial_vma_normalization_catches_fixed_overlap() {
        let request = MremapRequest {
            addr: VirtAddr::from(0x20_1000),
            old_size: PageSize::Size2M as usize - PAGE_SIZE_4K,
            new_size: PAGE_SIZE_4K,
            may_move: true,
            fixed: true,
            dont_unmap: false,
            new_addr: VirtAddr::from(0x40_0000),
        };
        let request = normalize_remap_request_geometry(request, PageSize::Size2M).unwrap();

        assert_eq!(
            fixed_destination_seal_probe_range(request, PageSize::Size2M as usize),
            Err(AxError::InvalidInput)
        );
    }
}
