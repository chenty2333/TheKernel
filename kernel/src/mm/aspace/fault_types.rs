//! Page-fault results, shared-folio PTE replacement plans and swapoff pages.

use super::*;

/// Acquires a shared-backing publication admission without retaining the mm
/// mutex while a THP alias mutation is in flight.  The caller must still take
/// the mm lock and validate its intended VMA transaction before publishing.
pub(crate) fn prepare_shared_alias_binding_lock_external(
    key: SharedBackingKey,
    aspace: &Arc<Mutex<AddrSpace>>,
) -> AxResult<PendingAliasLease> {
    loop {
        let address_space_id = aspace.lock().address_space_id();
        match PendingAliasLease::try_prepare(key, aspace, address_space_id) {
            Ok(pending) => return Ok(pending),
            Err(AxError::WouldBlock) => wait_for_alias_publication(key),
            Err(error) => return Err(error),
        }
    }
}

pub(super) type SharedFolioPteRun = ReplacedPteRun<X64PTE, PagingHandlerImpl>;

/// Fully prepared advisory-sidecar publication for mremap.  Construction
/// works on detached maps so commit cannot allocate after VMA/PTE mutation.
pub(crate) struct PreparedMadviseSidecarRemap {
    pub(super) guard_ranges: BTreeMap<VirtAddr, VirtAddr>,
    pub(super) hwpoison_ranges: BTreeMap<VirtAddr, VirtAddr>,
    pub(super) free_pages: BTreeMap<VirtAddr, LazyFreePage>,
}

/// A detached P1 run held until every alias has published its matching PMD.
/// Dropping it commits the page-table half of the transaction; passing it to
/// rollback restores the exact PTE bytes, including accessed/dirty state.
pub(crate) struct SharedFolioPteReplacement {
    pub(super) start: VirtAddr,
    pub(super) run: SharedFolioPteRun,
}

/// A non-target shared alias kept at 4 KiB granularity while another alias
/// promotes the backing object.  It records the exact pre-promotion leaf so
/// a failed multi-mm publication can put the old backing back without ever
/// exposing a PTE to a frame retained only by `SharedFolio::old_pages`.
#[derive(Clone, Copy)]
pub(crate) struct SharedFolioPteRedirect {
    pub(super) vaddr: VirtAddr,
    pub(super) old_paddr: PhysAddr,
    pub(super) flags: MappingFlags,
    pub(super) backing_index: usize,
}

/// One alias switched from a shared compound PMD back to protected 4 KiB
/// leaves.  The old PMD can be restored until backing ownership commits.
pub(crate) struct SharedFolioDemotionReplacement {
    pub(crate) start: VirtAddr,
    pub(super) folio: PhysAddr,
    pub(crate) flags: MappingFlags,
}

pub(crate) struct PreparedSharedFolioPmdRedirect {
    pub(crate) start: VirtAddr,
    pub(crate) flags: MappingFlags,
    pub(super) frames: Vec<PhysAddr>,
    pub(super) tables: PreparedPageTableFrames,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageFaultResult {
    Handled,
    /// Internal only: a file-cache fill found that reclaim must run after the
    /// address-space mutex is released. `FaultSession` consumes this result
    /// and retries VMA validation; it is never exposed to user mode.
    Retry,
    Failed(PageFaultFailure),
}

/// Linux rusage classification for one successful missing-page resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageFaultKind {
    /// No backing-device read was needed.
    Minor,
    /// The backing file had to supply page contents.
    Major,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageFaultFailure {
    AddressNotMapped,
    AccessDenied,
    BackingUnavailable,
    InternalInconsistency,
    OutOfMemory,
}

/// A retained software swap PTE captured outside swapoff's final MM lock
/// phase. The extra slot reference keeps backing stable while page-in frames
/// and page-table reservations are prepared.
pub(crate) struct SwapoffPage {
    pub(super) page: VirtAddr,
    pub(super) entry: crate::mm::SwapPte,
}

pub(crate) struct PreparedSwapoffPage {
    pub(super) page: VirtAddr,
    pub(super) entry: crate::mm::SwapPte,
    pub(super) prepared: PreparedCowPage,
}

impl SwapoffPage {
    pub(crate) fn prepare(self) -> AxResult<PreparedSwapoffPage> {
        let mut prepared = PreparedCowPage::try_new()?;
        prepared.reserve_max_table_frames()?;
        // SAFETY: the fill reads through the checked VM usercopy primitive, whose `Ok(())` means
        // every byte of the 4 KiB slice was initialized, as `prepare_uninitialized` requires;
        // `bytes` is exactly `PAGE_SIZE_4K` long.
        unsafe {
            prepared.prepare_uninitialized(|bytes| {
                let page = core::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast(), PAGE_SIZE_4K);
                crate::mm::read(self.entry, page)
            })?;
        }
        let result = PreparedSwapoffPage {
            page: self.page,
            entry: self.entry,
            prepared,
        };
        core::mem::forget(self);
        Ok(result)
    }
}

impl Drop for SwapoffPage {
    fn drop(&mut self) {
        let _ = crate::mm::release(self.entry);
    }
}

impl Drop for PreparedSwapoffPage {
    fn drop(&mut self) {
        // This is the temporary preflight pin, distinct from the software
        // PTE reference which the commit path drops after publication.
        let _ = crate::mm::release(self.entry);
    }
}

pub(super) fn classify_page_population(result: AxResult<usize>) -> PageFaultResult {
    match result {
        Ok(0) => PageFaultResult::Failed(PageFaultFailure::InternalInconsistency),
        Ok(_) => PageFaultResult::Handled,
        Err(err) if err.canonicalize() == AxError::ResourceBusy => PageFaultResult::Retry,
        Err(err) if err.canonicalize() == AxError::NoMemory => {
            PageFaultResult::Failed(PageFaultFailure::OutOfMemory)
        }
        Err(err)
            if matches!(
                err.canonicalize(),
                AxError::BadAddress | AxError::BadState | AxError::InvalidInput
            ) =>
        {
            PageFaultResult::Failed(PageFaultFailure::InternalInconsistency)
        }
        Err(_) => PageFaultResult::Failed(PageFaultFailure::BackingUnavailable),
    }
}

#[inline]
pub(super) fn synchronize_executable_publication(flags: MappingFlags) {
    if flags.contains(MappingFlags::EXECUTE) {
        drop(super::super::synchronize_icache());
    }
}

pub(super) fn adds_execute_permission(old_flags: MappingFlags, new_flags: MappingFlags) -> bool {
    new_flags.contains(MappingFlags::EXECUTE) && !old_flags.contains(MappingFlags::EXECUTE)
}

/// Builds the child-side backend for one `VM_WIPEONFORK` fragment.
///
/// `dup_mmap()` copies `vm_flags` wholesale (`mm/mmap.c:1130-1140`), so a
/// droppable parent VMA yields a droppable child VMA as well; the child keeps
/// refusing `MADV_KEEPONFORK`/`MADV_DODUMP` and keeps discarding its leaves
/// under reclaim.  Only the *pages* are replaced by a fresh zero page.
pub(super) fn wipe_on_fork_backend(
    start: VirtAddr,
    page_size: PageSize,
    sealed: bool,
    droppable: bool,
) -> Backend {
    let mut backend = Backend::new_alloc(start, page_size);
    if sealed {
        backend.set_sealed();
    }
    if droppable {
        backend = backend.with_droppable();
    }
    backend
}

pub(super) fn present_leaf_satisfies_fault(
    page_flags: MappingFlags,
    access_flags: PageFaultFlags,
) -> bool {
    page_flags.contains(access_flags)
}

/// Clamps one userfaultfd ioctl range to the address space that may contain
/// live VMAs. Current Linux permits the raw range to begin below
/// `mmap_min_addr`; only actual VMA intersections reach registration policy.
pub(super) fn uffd_vma_scan_range(
    range: PageRange,
    address_space_start: VirtAddr,
    address_space_end: VirtAddr,
) -> AxResult<VirtAddrRange> {
    let start = range.start().max(address_space_start.as_usize());
    let end = range.end().min(address_space_end.as_usize());
    if start >= end {
        return Err(AxError::InvalidInput);
    }
    Ok(VirtAddrRange::new(
        VirtAddr::from(start),
        VirtAddr::from(end),
    ))
}

/// The first MISSING-only profile does not advertise
/// `UFFD_FEATURE_MISSING_HUGETLBFS`.
///
/// Mapping snapshots deliberately use 4 KiB Linux policy geometry, so
/// accepting a larger backend leaf here would let a resolver or ordinary
/// fault populate outside one registered 4 KiB page. Reject it before any
/// registration-table mutation.
pub(super) fn validate_uffd_missing_backend_granule(page_size: PageSize) -> AxResult {
    if page_size == PageSize::Size4K {
        Ok(())
    } else {
        Err(AxError::InvalidInput)
    }
}
