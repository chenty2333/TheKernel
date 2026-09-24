//! AddrSpace: userfaultfd registration and transparent-huge-page collapse and split.

use super::*;

impl AddrSpace {
    /// Collapses one private COW PMD into a privately owned 2 MiB leaf,
    /// materializing absent anonymous or file-backed leaves directly in the
    /// prepared frame.
    ///
    /// The source leaves are first made read-only and observed through a TLB
    /// grace period, so copying cannot race a stale writable translation.  A
    /// new huge frame is then copied before the single PDE publication.  The
    /// VMA fragment keeps its lineage while changing only its COW granule;
    /// every detached 4 KiB frame and the former PTE table remain owned until
    /// the replacement's TLB grace completes.
    pub(crate) fn collapse_private_cow_2m(&mut self, start: VirtAddr) -> AxResult {
        if !self.forced_thp_collapse_allowed(start, COLLAPSE_2M_SIZE)
            || !self.collapse_2m_candidate_eligible(start, COLLAPSE_2M_SIZE)
        {
            return Err(AxError::InvalidInput);
        }
        self.check_no_user_io_pin_overlap(start, COLLAPSE_2M_SIZE, InvalidationReason::Remap)?;
        // This is the only fallible bookkeeping step after PDE publication;
        // admit it before changing either metadata or translations.
        let next_topology_generation = self.next_topology_generation()?;

        let source_backend = self
            .find_area(start)
            .ok_or(AxError::NoMemory)?
            .backend()
            .clone();
        let vma_flags = self.find_area(start).ok_or(AxError::NoMemory)?.flags();
        let metadata_plan = self.prepare_fragmented_cow_2m_page_size(start, PageSize::Size2M)?;
        let mut leaves = Vec::new();
        leaves
            .try_reserve_exact(COLLAPSE_2M_SIZE / PAGE_SIZE_4K)
            .map_err(|_| AxError::NoMemory)?;
        let mut source_slots = Vec::new();
        source_slots
            .try_reserve_exact(COLLAPSE_2M_SIZE / PAGE_SIZE_4K)
            .map_err(|_| AxError::NoMemory)?;

        let mut source_flags = None;
        for offset in (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K) {
            let vaddr = start + offset;
            match self.pt.query(vaddr) {
                Ok((paddr, flags, page_size)) => {
                    if page_size != PageSize::Size4K
                        || !PageSize::Size4K.is_aligned(paddr.as_usize())
                        || source_flags.is_some_and(|expected| expected != flags)
                    {
                        return Err(AxError::InvalidInput);
                    }
                    source_flags = Some(flags);
                    source_slots.push(Some(paddr));
                    leaves.push((vaddr, paddr, flags, page_size));
                }
                Err(PagingError::NotMapped) => {
                    if self.uffd_missing_registered_at(vaddr) {
                        return Err(AxError::ResourceBusy);
                    }
                    source_slots.push(None);
                }
                Err(_) => return Err(AxError::BadAddress),
            }
        }
        // For an entirely untouched anonymous VMA there is no PTE flag to
        // inherit; its VMA access contract becomes the new PMD leaf flags.
        let source_flags = source_flags.unwrap_or_else(|| {
            if vma_flags.contains(MappingFlags::WRITE) {
                vma_flags | MappingFlags::READ
            } else {
                vma_flags
            }
        });
        if !source_flags.contains(MappingFlags::WRITE) {
            return Err(AxError::InvalidInput);
        }
        let protected_flags = source_flags - MappingFlags::WRITE;

        // Long-term writable pins are tracked by physical frame precisely so
        // virtual remaps do not turn an unrelated pin into a global barrier.
        // Reject only this PMD's own source frames, before write-protecting
        // them or allocating the replacement frame.
        let pinned_frames = self.active_long_term_cow_frames()?;
        if leaves
            .iter()
            .any(|(_, frame, ..)| pinned_frames.binary_search(frame).is_ok())
            || any_frame_pinned(leaves.iter().map(|(_, frame, ..)| *frame))
        {
            return Err(AxError::ResourceBusy);
        }

        // The address-space lock prevents ordinary page-table mutation, but a
        // running CPU can still hold a writable translation. Revoke it before
        // taking the source snapshot.
        let write_protection_failed = {
            let mut cursor = self.pt.cursor();
            let mut failed = false;
            for (vaddr, ..) in &leaves {
                match cursor.protect(*vaddr, protected_flags) {
                    Ok(PageSize::Size4K) => {}
                    Ok(_) | Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            failed
        };
        if write_protection_failed {
            self.restore_collapse_2m_source_permissions(&leaves)?;
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());

        let mut prepared = match source_backend.prepare_collapse_2m_frame(start, &source_slots) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.restore_collapse_2m_source_permissions(&leaves)?;
                return Err(error);
            }
        };

        let replacement = match prepared.frame() {
            Ok(replacement) => replacement,
            Err(error) => {
                self.restore_collapse_2m_source_permissions(&leaves)?;
                return Err(error);
            }
        };
        // Commit is one allocation-free tree swap.  The guard retains the
        // exact old fragments and rolls them back if PDE publication fails.
        let metadata = match metadata_plan.commit(&mut self.areas) {
            Ok(metadata) => metadata,
            Err(error) => {
                {
                    let mut cursor = self.pt.cursor();
                    for (vaddr, _, flags, _) in &leaves {
                        if !matches!(cursor.protect(*vaddr, *flags), Ok(PageSize::Size4K)) {
                            return Err(AxError::BadState);
                        }
                    }
                }
                drop(self.tlb.synchronize_after_mutation());
                return Err(AxError::from(error));
            }
        };
        let replaced = {
            let mut cursor = self.pt.cursor();
            match cursor.replace_2m_pte_run(start, replacement, source_flags) {
                Ok(replaced) => Ok(Some(replaced)),
                // A completely untouched VMA need not have a P1 table yet.
                // Publish the prepared PMD directly in that case; the map
                // path constructs only unreachable intermediate tables before
                // linking the huge leaf.
                Err(PagingError::NotMapped) => cursor
                    .map(start, replacement, PageSize::Size2M, source_flags)
                    .map(|_| None)
                    .map_err(|_| AxError::BadState),
                Err(_) => Err(AxError::BadState),
            }
        };
        let replaced = match replaced {
            Ok(replaced) => replaced,
            Err(error) => {
                metadata.rollback();
                {
                    let mut cursor = self.pt.cursor();
                    for (vaddr, _, flags, _) in &leaves {
                        if !matches!(cursor.protect(*vaddr, *flags), Ok(PageSize::Size4K)) {
                            return Err(AxError::BadState);
                        }
                    }
                }
                drop(self.tlb.synchronize_after_mutation());
                return Err(error);
            }
        };
        metadata.finish();
        prepared.commit_frame();

        // `leaves` was checked above and belongs to this exact 4 KiB COW
        // backend, so retirement is now an infallible ownership conversion.
        let retired = source_backend
            .retire_collapsed_2m_sources(start, leaves)
            .expect("validated COW collapse leaves must be retireable");
        self.commit_topology_generation(next_topology_generation);

        // `replaced` owns the detached P1 table and `retired` owns the old
        // COW frames. Neither can be released before the PDE publication has
        // reached every CPU which could retain a former 4 KiB translation.
        let grace = self.synchronize_tlb_after_mutation();
        drop(retired);
        drop(replaced);
        drop(grace);
        Ok(())
    }

    /// Promotes a fully resident, physically contiguous shared/file 4 KiB
    /// run without changing its backing ownership.
    ///
    /// Unlike private anonymous collapse this must not copy into a new frame:
    /// doing so would sever MAP_SHARED visibility or the file-cache's
    /// writeback and eviction identity.  A naturally contiguous cache/shmem
    /// run can instead be represented directly by a PDE referring to the
    /// exact same frames.  Non-contiguous or sparse runs remain ineligible.
    pub(crate) fn collapse_alias_preserving_2m(&mut self, start: VirtAddr) -> AxResult {
        if !PageSize::Size2M.is_aligned(start.as_usize()) {
            return Err(AxError::InvalidInput);
        }
        let end = start + COLLAPSE_2M_SIZE;
        if !self.forced_thp_collapse_allowed(start, COLLAPSE_2M_SIZE) {
            return Err(AxError::InvalidInput);
        }
        let area = self
            .find_area(start)
            .filter(|area| area.start() <= start && area.end() >= end)
            .ok_or(AxError::NoMemory)?;
        if !matches!(area.backend(), Backend::Shared(_) | Backend::File(_))
            || self.range_is_locked(start, COLLAPSE_2M_SIZE)
            || Self::interval_overlaps(&self.wipe_on_fork_ranges, start, end)
            || Self::interval_overlaps(&self.dontfork_ranges, start, end)
        {
            return Err(AxError::InvalidInput);
        }
        let range = PageRange::new(start.as_usize(), COLLAPSE_2M_SIZE, PAGE_SIZE_4K)
            .map_err(|_| AxError::InvalidInput)?;
        if self.uffd.as_ref().is_some_and(|state| {
            state
                .registrations
                .intersecting(self.address_space_id, range)
                .any(|registration| registration.mode().bits() & UffdRegisterMode::WP.bits() != 0)
        }) {
            return Err(AxError::InvalidInput);
        }

        let (base, flags, size) = self.pt.query(start).map_err(|error| match error {
            PagingError::NotMapped => AxError::NoMemory,
            _ => AxError::BadAddress,
        })?;
        if size != PageSize::Size4K || !PageSize::Size2M.is_aligned(base.as_usize()) {
            return Err(AxError::InvalidInput);
        }
        for offset in (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K) {
            let (paddr, leaf_flags, leaf_size) =
                self.pt.query(start + offset).map_err(|error| match error {
                    PagingError::NotMapped => AxError::NoMemory,
                    _ => AxError::BadAddress,
                })?;
            let expected = base + offset;
            if leaf_size != PageSize::Size4K || leaf_flags != flags || paddr != expected {
                return Err(AxError::InvalidInput);
            }
        }
        let replaced = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_pte_run(start, base, flags)
        }
        .map_err(|error| match error {
            PagingError::NoMemory => AxError::NoMemory,
            _ => AxError::BadState,
        })?;
        // The detached P1 table is the only retired object.  The data frames
        // remain owned by SharedPages/CachedFile throughout this transaction.
        let grace = self.synchronize_tlb_after_mutation();
        drop(replaced);
        drop(grace);
        Ok(())
    }

    /// Returns every virtual alias of one byte range in a shared backing.
    /// The caller owns the global alias-mutation reservation, so the set
    /// remains stable until all participant mm locks are released.
    pub(crate) fn shared_backing_alias_ranges(
        &self,
        pages: &Arc<SharedPages>,
        backing_start: usize,
        backing_length: usize,
    ) -> AxResult<Vec<VirtAddrRange>> {
        let backing_end = backing_start
            .checked_add(backing_length)
            .ok_or(AxError::InvalidInput)?;
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            let Backend::Shared(shared) = area.backend() else {
                continue;
            };
            if !Arc::ptr_eq(shared.pages(), pages) {
                continue;
            }
            let area_backing_start = shared
                .backing_offset(area.start().as_usize())
                .ok_or(AxError::BadState)?;
            let area_backing_end = area_backing_start
                .checked_add(area.size())
                .ok_or(AxError::BadState)?;
            let overlap_start = backing_start.max(area_backing_start);
            let overlap_end = backing_end.min(area_backing_end);
            if overlap_start >= overlap_end {
                continue;
            }
            let virtual_start = area
                .start()
                .checked_add(overlap_start - area_backing_start)
                .ok_or(AxError::InvalidInput)?;
            let virtual_end = virtual_start
                .checked_add(overlap_end - overlap_start)
                .ok_or(AxError::InvalidInput)?;
            ranges.push(VirtAddrRange::new(virtual_start, virtual_end));
        }
        Ok(ranges)
    }

    /// Preflights and detaches resident PTEs for shared-backing aliases while
    /// retaining the VMA. The returned boolean tells the caller whether a TLB
    /// grace period is required before freeing the backing frames.
    pub(crate) fn detach_shared_backing_range(
        &mut self,
        pages: &Arc<SharedPages>,
        backing_start: usize,
        backing_length: usize,
    ) -> AxResult<bool> {
        let ranges = self.shared_backing_alias_ranges(pages, backing_start, backing_length)?;
        self.preflight_shared_backing_detach(&ranges)?;
        self.detach_preflighted_shared_backing_ranges(&ranges)
    }

    pub(crate) fn preflight_shared_backing_detach(&self, ranges: &[VirtAddrRange]) -> AxResult<()> {
        for range in ranges {
            let area = self.areas.find(range.start).ok_or(AxError::BadState)?;
            BackendOps::preflight_unmap(area.backend(), *range, &self.pt)?;
        }
        Ok(())
    }

    pub(crate) fn detach_preflighted_shared_backing_ranges(
        &mut self,
        ranges: &[VirtAddrRange],
    ) -> AxResult<bool> {
        if ranges.is_empty() {
            return Ok(false);
        }
        let mut cursor = self.pt.cursor();
        for range in ranges.iter().copied() {
            // SharedPages owns the frames, so the drained leaf descriptors do
            // not carry a separate retirement resource.
            drop(cursor.drain_mapped_leaves(range.start, range.size())?);
        }
        cursor.flush();
        Ok(true)
    }

    /// Finds every PMD-aligned mapping of one shared backing folio in this
    /// address space.  A non-PMD alias of the same 2 MiB backing must make the
    /// whole promotion ineligible: otherwise its old PTEs would keep pointing
    /// at the pre-folio frames after the shared backing is switched.
    pub(crate) fn shared_folio_alias_starts(
        &self,
        pages: &Arc<SharedPages>,
        start_index: usize,
    ) -> AxResult<Vec<VirtAddr>> {
        let backing_start = start_index
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(AxError::InvalidInput)?;
        let backing_end = backing_start
            .checked_add(COLLAPSE_2M_SIZE)
            .ok_or(AxError::InvalidInput)?;
        let mut starts = Vec::new();
        starts
            .try_reserve_exact(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            let Some(shared) = area.backend().shared_pages() else {
                continue;
            };
            if !Arc::ptr_eq(shared, pages) {
                continue;
            }
            let backend_start = match area.backend() {
                Backend::Shared(shared) => shared
                    .backing_offset(area.start().as_usize())
                    .ok_or(AxError::BadState)?,
                _ => unreachable!("shared pages originate only from SharedBackend"),
            };
            let backend_end = backend_start
                .checked_add(area.size())
                .ok_or(AxError::BadState)?;
            if backing_start >= backend_end || backing_end <= backend_start {
                continue;
            }
            // A partial overlap is still an alias of pages we are about to
            // replace, but cannot be made into a PMD without changing its VMA
            // geometry. Reject before any folio or PTE publication.
            if backing_start < backend_start || backing_end > backend_end {
                return Err(AxError::InvalidInput);
            }
            let start = area.start() + (backing_start - backend_start);
            if !PageSize::Size2M.is_aligned(start.as_usize()) {
                return Err(AxError::InvalidInput);
            }
            starts.push(start);
        }
        Ok(starts)
    }

    /// Validates an alias P1 run against the shared backing before its folio
    /// is promoted.  It performs every fallible VMA/UFFD/pin/PTE check while
    /// the old mapping remains live; publication below is then one PMD store.
    pub(crate) fn preflight_shared_folio_collapse_2m(
        &self,
        start: VirtAddr,
        pages: &Arc<SharedPages>,
        start_index: usize,
    ) -> AxResult<MappingFlags> {
        let end = start + COLLAPSE_2M_SIZE;
        let area = self
            .find_area(start)
            .filter(|area| area.start() <= start && area.end() >= end)
            .ok_or(AxError::NoMemory)?;
        if self.range_is_locked(start, COLLAPSE_2M_SIZE)
            || Self::interval_overlaps(&self.wipe_on_fork_ranges, start, end)
            || Self::interval_overlaps(&self.dontfork_ranges, start, end)
        {
            return Err(AxError::InvalidInput);
        }
        let Some(mapped_pages) = area.backend().shared_pages() else {
            return Err(AxError::InvalidInput);
        };
        if !Arc::ptr_eq(mapped_pages, pages) {
            return Err(AxError::BadState);
        }
        self.check_no_user_io_pin_overlap(start, COLLAPSE_2M_SIZE, InvalidationReason::Remap)?;
        let range = PageRange::new(start.as_usize(), COLLAPSE_2M_SIZE, PAGE_SIZE_4K)
            .map_err(|_| AxError::InvalidInput)?;
        if self.uffd.as_ref().is_some_and(|state| {
            state
                .registrations
                .intersecting(self.address_space_id, range)
                .any(|registration| registration.mode().bits() & UffdRegisterMode::WP.bits() != 0)
        }) {
            return Err(AxError::InvalidInput);
        }
        let (first, flags, size) = self.pt.query(start).map_err(|error| match error {
            PagingError::NotMapped => AxError::NoMemory,
            _ => AxError::BadAddress,
        })?;
        if size != PageSize::Size4K || first != pages.paddr_at(start_index)? {
            return Err(AxError::InvalidInput);
        }
        for page in 0..(COLLAPSE_2M_SIZE / PAGE_SIZE_4K) {
            let address = start + page * PAGE_SIZE_4K;
            let (paddr, leaf_flags, leaf_size) =
                self.pt.query(address).map_err(|error| match error {
                    PagingError::NotMapped => AxError::NoMemory,
                    _ => AxError::BadAddress,
                })?;
            if leaf_size != PageSize::Size4K
                || leaf_flags != flags
                || paddr != pages.paddr_at(start_index + page)?
            {
                return Err(AxError::InvalidInput);
            }
        }
        Ok(flags)
    }

    /// Preflights all resident 4 KiB aliases of a shared backing PMD except
    /// the requesting PMD.  Unlike the target, these aliases may begin/end
    /// mid-PMD or cross VMA fragments: they retain P1 geometry and are merely
    /// redirected to the corresponding subpage of the new folio.
    pub(crate) fn preflight_shared_folio_redirects_2m(
        &self,
        pages: &Arc<SharedPages>,
        start_index: usize,
        target: Option<VirtAddr>,
    ) -> AxResult<Vec<SharedFolioPteRedirect>> {
        let backing_start = start_index
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(AxError::InvalidInput)?;
        let ranges = self.shared_backing_alias_ranges(pages, backing_start, COLLAPSE_2M_SIZE)?;
        let mut redirects = Vec::new();
        for range in ranges {
            // A P1 redirect changes the physical frame observable through
            // this alias.  It therefore has the same DMA/pin, UFFD-WP and
            // fork/locking invalidation constraints as the target PMD even
            // though its VMA is intentionally not promoted.
            self.check_no_user_io_pin_overlap(
                range.start,
                range.size(),
                InvalidationReason::Remap,
            )?;
            if self.range_is_locked(range.start, range.size())
                || Self::interval_overlaps(&self.wipe_on_fork_ranges, range.start, range.end)
                || Self::interval_overlaps(&self.dontfork_ranges, range.start, range.end)
            {
                return Err(AxError::InvalidInput);
            }
            let page_range = PageRange::new(range.start.as_usize(), range.size(), PAGE_SIZE_4K)
                .map_err(|_| AxError::InvalidInput)?;
            if self.uffd.as_ref().is_some_and(|state| {
                state
                    .registrations
                    .intersecting(self.address_space_id, page_range)
                    .any(|registration| {
                        registration.mode().bits() & UffdRegisterMode::WP.bits() != 0
                    })
            }) {
                return Err(AxError::InvalidInput);
            }
            let mut address = range.start;
            while address < range.end {
                if target
                    .is_some_and(|target| address >= target && address < target + COLLAPSE_2M_SIZE)
                {
                    address += PAGE_SIZE_4K;
                    continue;
                }
                let offset = self
                    .shared_backing_offset_at(address)
                    .ok_or(AxError::BadState)?;
                let index = offset / PAGE_SIZE_4K;
                match self.pt.query(address) {
                    Ok((paddr, flags, PageSize::Size4K)) => {
                        if paddr != pages.paddr_at(index)? {
                            return Err(AxError::BadState);
                        }
                        redirects.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                        redirects.push(SharedFolioPteRedirect {
                            vaddr: address,
                            old_paddr: paddr,
                            flags,
                            backing_index: index,
                        });
                    }
                    // Non-target PMDs are represented by a separately
                    // prepared transactional P1 redirect plan.
                    Ok((_, _, PageSize::Size2M)) => {}
                    Ok(_) => return Err(AxError::BadState),
                    Err(PagingError::NotMapped) => {}
                    Err(_) => return Err(AxError::BadAddress),
                }
                address += PAGE_SIZE_4K;
            }
        }
        Ok(redirects)
    }

    pub(crate) fn prepare_shared_folio_pmd_redirects_except(
        &self,
        pages: &Arc<SharedPages>,
        start_index: usize,
        target: Option<VirtAddr>,
    ) -> AxResult<Vec<PreparedSharedFolioPmdRedirect>> {
        let backing_start = start_index
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(AxError::InvalidInput)?;
        let ranges = self.shared_backing_alias_ranges(pages, backing_start, COLLAPSE_2M_SIZE)?;
        let expected = pages.paddr_at(start_index)?;
        let mut plans = Vec::new();
        for range in ranges {
            let mut start = range.start.align_down(PageSize::Size2M);
            while start < range.end {
                if target != Some(start)
                    && matches!(self.pt.query(start), Ok((paddr, _, PageSize::Size2M)) if paddr == expected)
                    && !plans
                        .iter()
                        .any(|plan: &PreparedSharedFolioPmdRedirect| plan.start == start)
                {
                    self.check_no_user_io_pin_overlap(
                        start,
                        COLLAPSE_2M_SIZE,
                        InvalidationReason::Remap,
                    )?;
                    let range = PageRange::new(start.as_usize(), COLLAPSE_2M_SIZE, PAGE_SIZE_4K)
                        .map_err(|_| AxError::InvalidInput)?;
                    if self.range_is_locked(start, COLLAPSE_2M_SIZE)
                        || Self::interval_overlaps(
                            &self.wipe_on_fork_ranges,
                            start,
                            start + COLLAPSE_2M_SIZE,
                        )
                        || Self::interval_overlaps(
                            &self.dontfork_ranges,
                            start,
                            start + COLLAPSE_2M_SIZE,
                        )
                        || self.uffd.as_ref().is_some_and(|state| {
                            state
                                .registrations
                                .intersecting(self.address_space_id, range)
                                .any(|r| r.mode().bits() & UffdRegisterMode::WP.bits() != 0)
                        })
                    {
                        return Err(AxError::InvalidInput);
                    }
                    let (_, flags, _) = self.pt.query(start).map_err(|_| AxError::BadState)?;
                    let mut frames = Vec::new();
                    frames
                        .try_reserve_exact(COLLAPSE_2M_SIZE / PAGE_SIZE_4K)
                        .map_err(|_| AxError::NoMemory)?;
                    plans.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    plans.push(PreparedSharedFolioPmdRedirect {
                        start,
                        flags,
                        frames,
                        tables: PreparedPageTableFrames::try_new(1)
                            .map_err(|_| AxError::NoMemory)?,
                    });
                }
                start = start + COLLAPSE_2M_SIZE;
            }
        }
        Ok(plans)
    }

    pub(crate) fn publish_shared_folio_pmd_redirect(
        &mut self,
        plan: &mut PreparedSharedFolioPmdRedirect,
        folio: PhysAddr,
    ) -> AxResult<SharedFolioDemotionReplacement> {
        plan.frames.clear();
        for offset in (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K) {
            plan.frames.push(folio + offset);
        }
        self.publish_shared_folio_demotion_2m(
            plan.start,
            &plan.frames,
            plan.flags,
            &mut plan.tables,
        )
    }

    pub(crate) fn write_protect_shared_folio_redirects(
        &mut self,
        redirects: &[SharedFolioPteRedirect],
    ) -> AxResult {
        let mut cursor = self.pt.cursor();
        for (index, redirect) in redirects.iter().enumerate() {
            if !redirect.flags.contains(MappingFlags::WRITE) {
                continue;
            }
            if !matches!(
                cursor.protect(redirect.vaddr, redirect.flags - MappingFlags::WRITE),
                Ok(PageSize::Size4K)
            ) {
                for restore in &redirects[..=index] {
                    if restore.flags.contains(MappingFlags::WRITE) {
                        cursor
                            .protect(restore.vaddr, restore.flags)
                            .map_err(|_| AxError::BadState)?;
                    }
                }
                cursor.flush();
                return Err(AxError::BadState);
            }
        }
        cursor.flush();
        Ok(())
    }

    pub(crate) fn publish_shared_folio_redirects(
        &mut self,
        redirects: &[SharedFolioPteRedirect],
        folio: PhysAddr,
    ) -> AxResult {
        let mut cursor = self.pt.cursor();
        for (index, redirect) in redirects.iter().enumerate() {
            let offset = redirect
                .backing_index
                .checked_mul(PAGE_SIZE_4K)
                .ok_or(AxError::InvalidInput)?;
            let paddr = folio + (offset % COLLAPSE_2M_SIZE);
            let flags = redirect.flags - MappingFlags::WRITE;
            if cursor.remap(redirect.vaddr, paddr, flags).is_err() {
                for rollback in &redirects[..=index] {
                    cursor
                        .remap(
                            rollback.vaddr,
                            rollback.old_paddr,
                            rollback.flags - MappingFlags::WRITE,
                        )
                        .map_err(|_| AxError::BadState)?;
                }
                cursor.flush();
                return Err(AxError::BadState);
            }
        }
        cursor.flush();
        Ok(())
    }

    pub(crate) fn rollback_shared_folio_redirects(
        &mut self,
        redirects: &[SharedFolioPteRedirect],
    ) -> AxResult {
        let mut cursor = self.pt.cursor();
        for redirect in redirects {
            if cursor
                .remap(
                    redirect.vaddr,
                    redirect.old_paddr,
                    redirect.flags - MappingFlags::WRITE,
                )
                .is_err()
            {
                return Err(AxError::BadState);
            }
        }
        cursor.flush();
        Ok(())
    }

    pub(crate) fn restore_shared_folio_redirect_permissions(
        &mut self,
        redirects: &[SharedFolioPteRedirect],
    ) -> AxResult {
        let mut cursor = self.pt.cursor();
        for redirect in redirects {
            if !redirect.flags.contains(MappingFlags::WRITE) {
                continue;
            }
            if !matches!(
                cursor.protect(redirect.vaddr, redirect.flags),
                Ok(PageSize::Size4K)
            ) {
                return Err(AxError::BadState);
            }
        }
        cursor.flush();
        Ok(())
    }

    pub(crate) fn publish_shared_folio_collapse_2m(
        &mut self,
        start: VirtAddr,
        folio: PhysAddr,
        flags: MappingFlags,
    ) -> AxResult<SharedFolioPteReplacement> {
        let source_flags = if flags.contains(MappingFlags::WRITE) {
            flags - MappingFlags::WRITE
        } else {
            flags
        };
        let run = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_pte_run(start, folio, source_flags)
        }
        .map_err(|error| match error {
            PagingError::NoMemory => AxError::NoMemory,
            _ => AxError::BadState,
        })?;
        // Publication is deliberately read-only.  The cross-mm transaction
        // is not committed until every PMD and P1 alias names the promoted
        // backing and has passed a global TLB grace period.  Restoring WRITE
        // here would let a later rollback race `demote_4k_folio()`'s copy.
        Ok(SharedFolioPteReplacement { start, run })
    }

    /// Revokes writable 4 KiB translations before a shared-folio snapshot.
    /// The address-space mutex alone does not evict translations that a CPU
    /// installed before this transaction started.
    pub(crate) fn write_protect_shared_folio_collapse_2m(
        &mut self,
        start: VirtAddr,
        flags: MappingFlags,
    ) -> AxResult {
        if !flags.contains(MappingFlags::WRITE) {
            return Ok(());
        }
        let protected = flags - MappingFlags::WRITE;
        let result = {
            let mut cursor = self.pt.cursor();
            let mut result = Ok(());
            for offset in (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K) {
                if !matches!(
                    cursor.protect(start + offset, protected),
                    Ok(PageSize::Size4K)
                ) {
                    result = Err(AxError::BadState);
                    break;
                }
            }
            result
        };
        if result.is_err() {
            self.restore_shared_folio_permissions_2m(start, flags)?;
            return result;
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn restore_shared_folio_permissions_2m(
        &mut self,
        start: VirtAddr,
        flags: MappingFlags,
    ) -> AxResult {
        if !flags.contains(MappingFlags::WRITE) {
            return Ok(());
        }
        {
            let mut cursor = self.pt.cursor();
            for offset in (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K) {
                if !matches!(cursor.protect(start + offset, flags), Ok(PageSize::Size4K)) {
                    return Err(AxError::BadState);
                }
            }
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn rollback_shared_folio_collapse_2m(
        &mut self,
        replacement: SharedFolioPteReplacement,
    ) -> AxResult {
        {
            let mut cursor = self.pt.cursor();
            cursor.rollback_2m_pte_replacement(replacement.start, replacement.run)
        }
        .map_err(|_| AxError::BadState)?;
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn commit_shared_folio_collapse_2m(
        &mut self,
        replacement: SharedFolioPteReplacement,
    ) {
        let grace = self.synchronize_tlb_after_mutation();
        drop(replacement);
        drop(grace);
    }

    /// Changes only the COW granule of every VMA fragment spanning one PMD.
    /// Fragment-local policies (THP, readahead, seals and retained mapping
    /// leases) remain attached to their own backend clone.
    pub(super) fn prepare_fragmented_cow_2m_page_size(
        &self,
        start: VirtAddr,
        target: PageSize,
    ) -> AxResult<memory_set::PreparedMetadataUpdate<Backend>> {
        let source = match target {
            PageSize::Size4K => PageSize::Size2M,
            PageSize::Size2M => PageSize::Size4K,
            _ => return Err(AxError::InvalidInput),
        };
        if !self.private_cow_fragments_cover(start, COLLAPSE_2M_SIZE, source) {
            return Err(AxError::InvalidInput);
        }

        self.areas
            .prepare_metadata_update_with_limit(
                start,
                COLLAPSE_2M_SIZE,
                |_| true,
                |backend| {
                    *backend = match target {
                        PageSize::Size4K => backend
                            .demoted_4k_backend()
                            .expect("preflighted huge COW fragment must demote"),
                        PageSize::Size2M => backend
                            .collapsed_2m_backend()
                            .expect("preflighted 4K COW fragment must restore huge metadata"),
                        _ => unreachable!("unsupported COW fragment page size was preflighted"),
                    };
                },
                MAX_VMA_FRAGMENTS,
            )
            .map_err(AxError::from)
    }

    pub(super) fn restore_collapse_2m_source_permissions(
        &mut self,
        leaves: &[(VirtAddr, PhysAddr, MappingFlags, PageSize)],
    ) -> AxResult {
        {
            let mut cursor = self.pt.cursor();
            for (vaddr, _, flags, _) in leaves {
                match cursor.protect(*vaddr, *flags) {
                    Ok(PageSize::Size4K) => {}
                    Ok(_) | Err(_) => return Err(AxError::BadState),
                }
            }
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    /// Demotes one private anonymous COW PMD into a prepared P1 run.
    ///
    /// A writable PMD is first revoked and flushed from the CPUs currently
    /// running this address space.  Only after that grace period is its
    /// content copied.  Thus no stale writable translation can modify the
    /// source while the replacement frames are being made.  Every failure
    /// before the PDE publication restores the exact original PMD flags;
    /// after publication the old PMD frame stays owned through a second,
    /// targeted TLB grace period.
    pub(crate) fn demote_private_cow_2m(&mut self, start: VirtAddr) -> AxResult {
        if !PageSize::Size2M.is_aligned(start.as_usize()) {
            return Err(AxError::InvalidInput);
        }
        self.check_no_user_io_pin_overlap(start, COLLAPSE_2M_SIZE, InvalidationReason::Remap)?;
        let source_backend = self
            .find_area(start)
            .filter(|area| area.start() <= start && area.end() > start)
            .ok_or(AxError::NoMemory)?
            .backend()
            .clone();
        if !source_backend.is_private_cow() || source_backend.page_size() != PageSize::Size2M {
            return Err(AxError::InvalidInput);
        }
        // MADV policy can split VMA metadata without demoting the PMD. Prove
        // that every fragment retains one lineage, one access contract and a
        // continuous private-COW backing cursor before changing the hardware
        // leaf beneath all of them.
        if !self.private_cow_fragments_cover(start, COLLAPSE_2M_SIZE, PageSize::Size2M) {
            return Err(AxError::InvalidInput);
        }
        let metadata_plan = self.prepare_fragmented_cow_2m_page_size(start, PageSize::Size4K)?;
        let (source_frame, source_flags, source_size) =
            self.pt.query_mapped(start).map_err(|error| match error {
                PagingError::NotMapped if self.uffd_missing_registered_at(start) => {
                    AxError::ResourceBusy
                }
                PagingError::NotMapped => AxError::NoMemory,
                _ => AxError::BadAddress,
            })?;
        if source_size != PageSize::Size2M || !PageSize::Size2M.is_aligned(source_frame.as_usize())
        {
            return Err(AxError::BadState);
        }
        let next_topology_generation = self.next_topology_generation()?;
        let mut tables = PreparedPageTableFrames::try_new(1).map_err(|_| AxError::NoMemory)?;

        // The address-space mutex serializes page-table writers, but CPUs
        // which ran this mm before we acquired it can retain a writable PMD
        // translation. Revoke WRITE and wait for precisely those CPUs before
        // sampling the source frame.
        let protected_flags = source_flags - MappingFlags::WRITE;
        let protected = {
            let mut cursor = self.pt.cursor();
            cursor.protect(start, protected_flags)
        };
        if !matches!(protected, Ok(PageSize::Size2M)) {
            // `protect` is expected to be all-or-nothing for one PMD. Still
            // restore the observed PMD exactly in case a malformed table made
            // the operation report after changing it.
            self.restore_demote_2m_source_permissions(start, source_flags)?;
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());

        let mut prepared = match source_backend.prepare_demote_2m_frames(source_frame) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.restore_demote_2m_source_permissions(start, source_flags)?;
                return Err(error);
            }
        };

        // All metadata allocation/splitting was admitted before write
        // revocation. Commit now is an allocation-free tree swap and retains
        // the exact huge-backend tree for failure rollback.
        let metadata = match metadata_plan.commit(&mut self.areas) {
            Ok(metadata) => metadata,
            Err(error) => {
                let restored = self.pt.cursor().protect(start, source_flags);
                if !matches!(restored, Ok(PageSize::Size2M)) {
                    return Err(AxError::BadState);
                }
                drop(self.tlb.synchronize_after_mutation());
                return Err(AxError::from(error));
            }
        };
        let published = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_huge_leaf_with_pte_run(
                start,
                prepared.frames(),
                source_flags,
                &mut tables,
            )
        };
        let published = match published {
            Ok(frame) => frame,
            Err(error) => {
                metadata.rollback();
                let restored = self.pt.cursor().protect(start, source_flags);
                if !matches!(restored, Ok(PageSize::Size2M)) {
                    return Err(AxError::BadState);
                }
                drop(self.tlb.synchronize_after_mutation());
                return Err(match error {
                    PagingError::NoMemory => AxError::NoMemory,
                    _ => AxError::BadState,
                });
            }
        };
        metadata.finish();
        debug_assert_eq!(published, source_frame);
        prepared.commit_frames();
        let retired = source_backend
            .retire_demoted_2m_source(start, source_frame, source_flags)
            .expect("validated huge COW leaf must be retireable");
        self.commit_topology_generation(next_topology_generation);
        let grace = self.synchronize_tlb_after_mutation();
        drop(retired);
        drop(grace);
        Ok(())
    }

    /// Expands an alias-preserving shared/file PDE back into 4 KiB leaves
    /// referring to the same backing/cache frames.  No data frame changes
    /// ownership, so this is safe for all MAP_SHARED aliases and cache pins.
    pub(crate) fn demote_alias_preserving_2m(&mut self, start: VirtAddr) -> AxResult {
        if !PageSize::Size2M.is_aligned(start.as_usize()) {
            return Err(AxError::InvalidInput);
        }
        let end = start + COLLAPSE_2M_SIZE;
        let area = self
            .find_area(start)
            .filter(|area| area.start() <= start && area.end() >= end)
            .ok_or(AxError::NoMemory)?;
        if !matches!(area.backend(), Backend::Shared(_) | Backend::File(_)) {
            return Err(AxError::InvalidInput);
        }
        let (source, flags, size) = self.pt.query_mapped(start).map_err(|error| match error {
            PagingError::NotMapped => AxError::NoMemory,
            _ => AxError::BadAddress,
        })?;
        if size != PageSize::Size2M || !PageSize::Size2M.is_aligned(source.as_usize()) {
            return Err(AxError::BadState);
        }
        let mut leaves = Vec::new();
        leaves
            .try_reserve_exact(COLLAPSE_2M_SIZE / PAGE_SIZE_4K)
            .map_err(|_| AxError::NoMemory)?;
        for offset in (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K) {
            leaves.push(source + offset);
        }
        let mut tables = PreparedPageTableFrames::try_new(1).map_err(|_| AxError::NoMemory)?;
        let published = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_huge_leaf_with_pte_run(start, &leaves, flags, &mut tables)
        }
        .map_err(|error| match error {
            PagingError::NoMemory => AxError::NoMemory,
            _ => AxError::BadState,
        })?;
        debug_assert_eq!(published, source);
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn preflight_shared_folio_demotion_2m(
        &self,
        start: VirtAddr,
        pages: &Arc<SharedPages>,
        start_index: usize,
    ) -> AxResult<MappingFlags> {
        let end = start + COLLAPSE_2M_SIZE;
        let area = self
            .find_area(start)
            .filter(|area| area.start() <= start && area.end() >= end)
            .ok_or(AxError::NoMemory)?;
        if !Arc::ptr_eq(
            area.backend().shared_pages().ok_or(AxError::InvalidInput)?,
            pages,
        ) {
            return Err(AxError::BadState);
        }
        self.check_no_user_io_pin_overlap(start, COLLAPSE_2M_SIZE, InvalidationReason::Remap)?;
        let range = PageRange::new(start.as_usize(), COLLAPSE_2M_SIZE, PAGE_SIZE_4K)
            .map_err(|_| AxError::InvalidInput)?;
        if self.uffd.as_ref().is_some_and(|state| {
            state
                .registrations
                .intersecting(self.address_space_id, range)
                .any(|registration| registration.mode().bits() & UffdRegisterMode::WP.bits() != 0)
        }) {
            return Err(AxError::InvalidInput);
        }
        let (folio, flags, size) = self.pt.query_mapped(start).map_err(|error| match error {
            PagingError::NotMapped => AxError::NoMemory,
            _ => AxError::BadAddress,
        })?;
        if size != PageSize::Size2M || folio != pages.paddr_at(start_index)? {
            return Err(AxError::BadState);
        }
        Ok(flags)
    }

    pub(crate) fn write_protect_shared_folio_demotion_2m(
        &mut self,
        start: VirtAddr,
        flags: MappingFlags,
    ) -> AxResult {
        if !flags.contains(MappingFlags::WRITE) {
            return Ok(());
        }
        let result = {
            let mut cursor = self.pt.cursor();
            cursor.protect(start, flags - MappingFlags::WRITE)
        };
        if !matches!(result, Ok(PageSize::Size2M)) {
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn restore_shared_folio_demotion_pmd_permissions(
        &mut self,
        start: VirtAddr,
        flags: MappingFlags,
    ) -> AxResult {
        if !flags.contains(MappingFlags::WRITE) {
            return Ok(());
        }
        let result = {
            let mut cursor = self.pt.cursor();
            cursor.protect(start, flags)
        };
        if !matches!(result, Ok(PageSize::Size2M)) {
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    pub(crate) fn publish_shared_folio_demotion_2m(
        &mut self,
        start: VirtAddr,
        frames: &[PhysAddr],
        flags: MappingFlags,
        tables: &mut PreparedPageTableFrames,
    ) -> AxResult<SharedFolioDemotionReplacement> {
        let protected = flags - MappingFlags::WRITE;
        let folio = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_huge_leaf_with_pte_run(start, frames, protected, tables)
        }
        .map_err(|error| match error {
            PagingError::NoMemory => AxError::NoMemory,
            _ => AxError::BadState,
        })?;
        Ok(SharedFolioDemotionReplacement {
            start,
            folio,
            flags,
        })
    }

    pub(crate) fn rollback_shared_folio_demotion_2m(
        &mut self,
        replacement: SharedFolioDemotionReplacement,
    ) -> AxResult {
        let start = replacement.start;
        let flags = replacement.flags;
        self.rollback_shared_folio_demotion_2m_protected(replacement)?;
        self.restore_shared_folio_demotion_pmd_permissions(start, flags)
    }

    /// Restores the old huge leaf while keeping its original WRITE bit
    /// revoked.  Cross-mm shared-folio rollback uses this phase before it
    /// restores the base-page backing; permission restoration is a separate,
    /// final transaction edge after the backing copy and TLB grace.
    pub(crate) fn rollback_shared_folio_demotion_2m_protected(
        &mut self,
        replacement: SharedFolioDemotionReplacement,
    ) -> AxResult {
        let protected = replacement.flags - MappingFlags::WRITE;
        let run = {
            let mut cursor = self.pt.cursor();
            cursor.replace_2m_pte_run(replacement.start, replacement.folio, protected)
        }
        .map_err(|_| AxError::BadState)?;
        let grace = self.synchronize_tlb_after_mutation();
        drop(run);
        drop(grace);
        Ok(())
    }

    /// Restores the source PMD after a demotion failure which occurred before
    /// replacement publication.  The target is one exact leaf, so restoring
    /// its original hardware flags is a single page-table mutation followed
    /// by the same targeted TLB grace used for write revocation.
    pub(super) fn restore_demote_2m_source_permissions(
        &mut self,
        start: VirtAddr,
        source_flags: MappingFlags,
    ) -> AxResult {
        let restored = {
            let mut cursor = self.pt.cursor();
            cursor.protect(start, source_flags)
        };
        if !matches!(restored, Ok(PageSize::Size2M)) {
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());
        Ok(())
    }

    /// Ensures that mutations which operate at page granularity never leave a
    /// private anonymous huge COW mapping or an alias-preserving shared/file
    /// huge mapping behind them.
    ///
    /// Call this after the operation's non-MM admission gates, but before it
    /// prepares VMA/PTE mutations or observes individual PTEs.
    pub(crate) fn ensure_4k_granularity(&mut self, start: VirtAddr, size: usize) -> AxResult {
        self.validate_region(start, size)?;
        if size == 0 {
            return Ok(());
        }

        let end = start + size;
        let mut candidate = VirtAddr::from(start.as_usize() & !(COLLAPSE_2M_SIZE - 1));
        while candidate < end {
            let demote_private = self.areas.find(candidate).is_some_and(|area| {
                area.start() <= candidate
                    && area.end() > candidate
                    && area.backend().is_private_cow()
                    && area.backend().page_size() == PageSize::Size2M
            }) && self
                .pt
                .query_mapped(candidate)
                .is_ok_and(|(_, _, page_size)| page_size == PageSize::Size2M);
            if demote_private {
                self.demote_private_cow_2m(candidate)?;
            } else {
                let demote_alias = self.areas.find(candidate).is_some_and(|area| {
                    area.start() <= candidate
                        && area.end() >= candidate + COLLAPSE_2M_SIZE
                        && matches!(area.backend(), Backend::Shared(_) | Backend::File(_))
                }) && self
                    .pt
                    .query_mapped(candidate)
                    .is_ok_and(|(_, _, page_size)| page_size == PageSize::Size2M);
                if demote_alias {
                    if let (Some(pages), Some(offset)) = (
                        self.shared_pages_at(candidate),
                        self.shared_backing_offset_at(candidate),
                    ) && pages.page_size() == PageSize::Size4K
                        && offset.is_multiple_of(COLLAPSE_2M_SIZE)
                        && pages.has_4k_folio(offset / PAGE_SIZE_4K)
                    {
                        // A compound shmem folio owns one set of former
                        // 4 KiB frames for every mm alias.  Its caller
                        // must use the ordered cross-mm transaction.
                        return Err(AxError::BadState);
                    }
                    self.demote_alias_preserving_2m(candidate)?;
                }
            }
            candidate = candidate
                .checked_add(COLLAPSE_2M_SIZE)
                .ok_or(AxError::InvalidInput)?;
        }
        Ok(())
    }
}
