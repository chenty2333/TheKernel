//! Linux-visible mremap planning and address-space transactions.

use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::{MappingFlags, PageSize};
#[cfg(target_arch = "x86_64")]
use axtask::current;
use linux_raw_sys::general::RLIMIT_MEMLOCK;
use memory_addr::{MemoryAddr, VirtAddr, VirtAddrRange};
use memory_set::MappingLineage;
use thekernel_linux_mm::{MemlockLimit, MemlockPlan, PageRange as LinuxPageRange, RemapGeometry};

use crate::{
    mm::{
        AddrSpace, Backend, BackendOps, DeferredUffdWake, ExistingLineageMapError,
        LockExternalUffdOutcome, checked_align_up,
    },
    syscall::ensure_4k_granularity_across_aliases,
    task::{AsThread, ProcessData},
};

/// Reconciliation runs under the mm mutex, but a task's live CET context is
/// scheduler-owned.  Consume the invalidation receipt only after releasing
/// that mutex, and only for the calling task (remote CLONE_VM owners are
/// repaired when they next execute a VMA-mutating path or exit).
#[cfg(target_arch = "x86_64")]
fn clear_current_cet_if_invalidated(invalidated: &[u32]) {
    let curr = current();
    if invalidated.contains(&curr.as_thread().kernel_tid()) {
        crate::task::reset_current_user_cet_state();
    }
}

#[derive(Clone)]
struct RemapSegment {
    start: VirtAddr,
    size: usize,
    flags: MappingFlags,
    backend: Backend,
    lineage: MappingLineage,
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
        let relocated = if preserve_backend_identity {
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
    new_addr: VirtAddr,
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
            aspace
                .find_kernel_area(
                    request.addr,
                    request.new_size,
                    VirtAddrRange::new(aspace.base(), aspace.end()),
                    page_size as usize,
                )
                .ok_or(AxError::NoMemory)?
        };

        let duplicated_locked = aspace.locked_bytes_in_range(request.addr, request.new_size);
        if duplicated_locked != 0 {
            check_mremap_locked_growth_limit(proc_data, has_ipc_lock, aspace, duplicated_locked)?;
        }
        return Ok((
            request,
            RemapPlan::Duplicate {
                source_segments,
                destination,
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
        && range_is_free(aspace, after, grow, page_size as usize)
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
        aspace
            .find_kernel_area(
                request.addr,
                request.new_size,
                VirtAddrRange::new(aspace.base(), aspace.end()),
                page_size as usize,
            )
            .ok_or(AxError::NoMemory)?
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

fn prepare_remap_plan(
    plan: RemapPlan,
    request: MremapRequest,
    aspace_handle: &Arc<axsync::Mutex<AddrSpace>>,
) -> AxResult<PreparedRemapPlan> {
    debug_assert!(!request.fixed);
    match plan {
        RemapPlan::Duplicate {
            source_segments,
            destination,
        } => {
            let destination_segments = prepare_relocated_segments(
                aspace_handle,
                request.addr,
                request.new_size,
                destination,
                request.new_size,
                &source_segments,
                false,
            )?;
            Ok(PreparedRemapPlan::Duplicate {
                source_segments,
                destination_segments,
                destination,
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
                ..
            } => {
                remap_segments_match(aspace, request.addr, request.new_size, source_segments)
                    && range_is_free(
                        aspace,
                        *destination,
                        request.new_size,
                        source_segments[0].backend.page_size() as usize,
                    )
            }
            Self::GrowInPlace {
                source_segments, ..
            } => {
                let grow = request.new_size.saturating_sub(request.old_size);
                remap_segments_match(aspace, request.addr, request.old_size, source_segments)
                    && range_is_free(
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
                    && range_is_free(
                        aspace,
                        *destination,
                        request.new_size,
                        source_segments[0].backend.page_size() as usize,
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
) -> LockExternalUffdOutcome<(isize, Vec<u32>), AxError> {
    debug_assert!(!request.fixed);
    let mut wake = DeferredUffdWake::empty();
    let outcome = (|| match prepared {
        PreparedRemapPlan::Duplicate {
            source_segments: _,
            destination_segments,
            destination,
        } => {
            let duplicated_locked = aspace.locked_bytes_in_range(request.addr, request.new_size);
            if duplicated_locked != 0 {
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
                primary.flags,
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
                            primary_flags,
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
    })();
    let outcome = outcome.map(|destination| {
        // The registry belongs to this mm, not the caller's ProcessData:
        // reconcile every CLONE_VM owner before releasing the VMA lock.
        #[cfg(target_arch = "x86_64")]
        let invalidated = aspace.reconcile_cet_default_shadow_stacks_after_mremap(
            request.addr,
            request.old_size,
            VirtAddr::from(destination as usize),
        );
        #[cfg(not(target_arch = "x86_64"))]
        let invalidated = Vec::new();
        (destination, invalidated)
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
) -> LockExternalUffdOutcome<(isize, Vec<u32>), AxError> {
    let mut wake = DeferredUffdWake::empty();
    let outcome = (|| match plan {
        RemapPlan::Return(address) => Ok(address.as_usize() as isize),
        RemapPlan::Shrink { start, size } => {
            wake.merge(aspace.unmap(start, size)?);
            proc_data.clear_mempolicy_range(start.as_usize(), size);
            Ok(request.addr.as_usize() as isize)
        }
        RemapPlan::Duplicate {
            source_segments,
            destination,
        } => {
            let duplicated_locked = aspace.locked_bytes_in_range(request.addr, request.new_size);
            if duplicated_locked != 0 {
                check_mremap_locked_growth_limit(
                    proc_data,
                    has_ipc_lock,
                    aspace,
                    duplicated_locked,
                )?;
            }
            let staged_fragments = source_segments.len();
            let duplicate = if request.fixed {
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
                            )
                        },
                    )
                    .map_err(|error| {
                        if error.mapping_changed() {
                            proc_data
                                .clear_mempolicy_range(destination.as_usize(), request.new_size);
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
                        )
                    },
                )
            };
            let (duplicate, transaction_wake) = duplicate.into_parts();
            wake.merge(transaction_wake);
            duplicate?;
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
                primary.flags,
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
                                    primary.flags,
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
                            proc_data
                                .clear_mempolicy_range(destination.as_usize(), request.new_size);
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
                                primary.flags,
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
    })();
    let outcome = outcome.map(|destination| {
        #[cfg(target_arch = "x86_64")]
        let invalidated = aspace.reconcile_cet_default_shadow_stacks_after_mremap(
            request.addr,
            request.old_size,
            VirtAddr::from(destination as usize),
        );
        #[cfg(not(target_arch = "x86_64"))]
        let invalidated = Vec::new();
        (destination, invalidated)
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
            let invalidated = aspace.reconcile_cet_default_shadow_stacks_after_mremap(
                request.addr, request.old_size, request.addr,
            );
            drop(aspace);
            wake.finish();
            #[cfg(target_arch = "x86_64")]
            clear_current_cet_if_invalidated(&invalidated);
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

    let prepared = prepare_remap_plan(plan, request, &aspace_handle)?;
    let mut aspace = super::lock_mm_diagnosed!(aspace_handle, MremapOptimisticCommit);
    if !proc_data.image_matches(&aspace_handle) || !prepared.revalidate(&aspace, request) {
        return Ok(OptimisticRemapOutcome::Retry);
    }
    let committed = commit_prepared_remap(&mut aspace, proc_data, has_ipc_lock, request, prepared);
    drop(aspace);
    let (result, invalidated) = committed.finish()?;
    #[cfg(target_arch = "x86_64")]
    clear_current_cet_if_invalidated(&invalidated);
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
        // Keep the locked and optimistic paths identical: partial move and
        // MREMAP_DONTUNMAP duplication must never slice one private huge leaf.
        aspace.ensure_4k_granularity(request.addr, source_size)?;
        let (request, plan) = build_remap_plan(&aspace, proc_data, has_ipc_lock, request)?;
        match plan {
            RemapPlan::Return(address) => return Ok(address.as_usize() as isize),
            RemapPlan::Shrink { start, size } => {
                let wake = aspace.unmap(start, size)?;
                proc_data.clear_mempolicy_range(start.as_usize(), size);
                #[cfg(target_arch = "x86_64")]
                let invalidated = aspace.reconcile_cet_default_shadow_stacks_after_mremap(
                    request.addr, request.old_size, request.addr,
                );
                drop(aspace);
                wake.finish();
                #[cfg(target_arch = "x86_64")]
                clear_current_cet_if_invalidated(&invalidated);
                return Ok(request.addr.as_usize() as isize);
            }
            _ => {}
        }
        let committed = commit_locked_remap(
            &mut aspace,
            &aspace_handle,
            proc_data,
            has_ipc_lock,
            request,
            plan,
        );
        drop(aspace);
        let (result, invalidated) = committed.finish()?;
        #[cfg(target_arch = "x86_64")]
        clear_current_cet_if_invalidated(&invalidated);
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
    new_addr: VirtAddr,
) -> AxResult<isize> {
    const OPTIMISTIC_RETRIES: usize = 2;

    let request = MremapRequest {
        addr,
        old_size,
        new_size,
        may_move,
        fixed,
        new_addr,
    };
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
            new_addr: VirtAddr::from(0x40_0000),
        };
        let request = normalize_remap_request_geometry(request, PageSize::Size2M).unwrap();

        assert_eq!(
            fixed_destination_seal_probe_range(request, PageSize::Size2M as usize),
            Err(AxError::InvalidInput)
        );
    }
}
