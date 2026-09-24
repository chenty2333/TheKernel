//! AddrSpace: fork fragments, relative NUMA policy, VMA flags and mlock.

use super::*;

impl AddrSpace {
    /// Linux mseal requires a mapped, gap-free range. The metadata transaction
    /// splits boundary VMAs before setting VM_SEALED, with no PTE mutation.
    pub(crate) fn seal(&mut self, start: VirtAddr, size: usize) -> AxResult {
        self.validate_region(start, size)?;
        if size == 0 {
            return Ok(());
        }
        if !range_is_fully_mapped(&self.areas, start, size) {
            return Err(AxError::NoMemory);
        }
        if self
            .areas
            .iter_overlapping(VirtAddrRange::new(start, start + size))
            .all(|area| area.backend().is_sealed())
        {
            return Ok(());
        }
        let next_topology_generation = self.next_topology_generation()?;
        let updated = self.areas.update_metadata_with_limit(
            start,
            size,
            |backend| !backend.is_sealed(),
            Backend::set_sealed,
            MAX_VMA_FRAGMENTS,
        );
        match updated {
            Ok(()) => {
                self.commit_topology_generation(next_topology_generation);
                Ok(())
            }
            Err(error) => {
                let (error, changed) = error.into_parts();
                if changed {
                    self.commit_topology_generation(next_topology_generation);
                }
                Err(AxError::from(error))
            }
        }
    }

    /// Installs Linux VM_RAND_READ/VM_SEQ_READ policy on exactly this VMA
    /// range.  `update_metadata_with_limit` owns boundary splitting and later
    /// re-merging; keeping the policy in `MappingStatus` makes fork and
    /// mremap preserve it without an address-keyed compatibility side table.
    pub(crate) fn set_madvise_readahead(
        &mut self,
        start: VirtAddr,
        size: usize,
        policy: MadviseReadahead,
    ) -> AxResult {
        self.validate_region(start, size)?;
        if size == 0 {
            return Ok(());
        }
        if !range_is_fully_mapped(&self.areas, start, size) {
            return Err(AxError::NoMemory);
        }
        let range = VirtAddrRange::new(start, start + size);
        if self
            .areas
            .iter_overlapping(range)
            .all(|area| area.backend().madvise_readahead() == policy)
        {
            return Ok(());
        }

        let next_topology_generation = self.next_topology_generation()?;
        let updated = self.areas.update_metadata_with_limit(
            start,
            size,
            |backend| backend.madvise_readahead() != policy,
            |backend| backend.set_madvise_readahead(policy),
            MAX_VMA_FRAGMENTS,
        );
        match updated {
            Ok(()) => {
                self.commit_topology_generation(next_topology_generation);
                Ok(())
            }
            Err(error) => {
                let (error, changed) = error.into_parts();
                if changed {
                    self.commit_topology_generation(next_topology_generation);
                }
                Err(AxError::from(error))
            }
        }
    }

    /// Installs VM_HUGEPAGE/VM_NOHUGEPAGE metadata without touching present
    /// page-table geometry. In particular, NOHUGEPAGE never demotes an
    /// existing transparent huge leaf merely to split the VMA policy.
    pub(crate) fn set_madvise_thp(
        &mut self,
        start: VirtAddr,
        size: usize,
        policy: MadviseThp,
    ) -> AxResult {
        self.validate_region(start, size)?;
        if size == 0 {
            return Ok(());
        }
        if !range_is_fully_mapped(&self.areas, start, size) {
            return Err(AxError::NoMemory);
        }
        let range = VirtAddrRange::new(start, start + size);
        if self
            .areas
            .iter_overlapping(range)
            .all(|area| area.backend().madvise_thp() == policy)
        {
            return Ok(());
        }

        let next_topology_generation = self.next_topology_generation()?;
        let updated = self.areas.update_metadata_with_limit(
            start,
            size,
            |backend| backend.madvise_thp() != policy,
            |backend| backend.set_madvise_thp(policy),
            MAX_VMA_FRAGMENTS,
        );
        match updated {
            Ok(()) => {
                self.commit_topology_generation(next_topology_generation);
                Ok(())
            }
            Err(error) => {
                let (error, changed) = error.into_parts();
                if changed {
                    self.commit_topology_generation(next_topology_generation);
                }
                Err(AxError::from(error))
            }
        }
    }

    pub(crate) const fn thp_disable_mode(&self) -> ThpDisableMode {
        self.thp_disable_mode
    }

    pub(crate) fn set_thp_disable_mode(&mut self, mode: ThpDisableMode) {
        self.thp_disable_mode = mode;
    }

    pub(crate) fn check_no_seal_overlap(&self, start: VirtAddr, size: usize) -> AxResult {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        if self
            .areas
            .iter_overlapping(VirtAddrRange::new(start, end))
            .any(|area| area.backend().is_sealed())
        {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(())
    }

    /// mremap checks the initially looked-up VMA's seal before validating the
    /// rest of its source geometry. Leave an unmapped address to the normal
    /// source-range validator so it retains Linux's EFAULT/ENOMEM mapping.
    pub(crate) fn check_vma_at_not_sealed(&self, address: VirtAddr) -> AxResult {
        if self
            .find_area(address)
            .is_some_and(|area| area.backend().is_sealed())
        {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(())
    }

    /// `can_modify_vma_madv()` in Linux 6.12.103 only rejects discard-style
    /// advice for sealed, read-only private anonymous VMAs.
    pub(crate) fn sealed_ro_anon_in_range(&self, start: VirtAddr, size: usize) -> bool {
        let Some(end) = start.checked_add(size) else {
            return false;
        };
        self.areas
            .iter_overlapping(VirtAddrRange::new(start, end))
            .any(|area| {
                area.backend().is_sealed()
                    && area.backend().is_private_anonymous()
                    && !area.flags().contains(MappingFlags::WRITE)
            })
    }

    pub(super) fn fork_fragment_count(&self) -> AxResult<usize> {
        let mut count = 0usize;
        for area in self.areas.iter() {
            if area
                .backend()
                .file_like_mapping()
                .is_some_and(|mapping| mapping.excludes_fork_and_dump())
            {
                continue;
            }
            let mut cursor = area.start();
            while cursor < area.end() {
                if let Some(dontfork_end) =
                    Self::interval_end_covering(&self.dontfork_ranges, cursor)
                {
                    cursor = dontfork_end.min(area.end());
                    continue;
                }

                let mut segment_end = area.end();
                // `VM_DROPPABLE` is derived, not annotated: Linux keeps the
                // whole VMA out of the child by way of the `VM_WIPEONFORK`
                // flag it set at `mm/mmap.c:533`, so a droppable area needs no
                // sidecar entry and contributes no fragment boundary of its
                // own.
                if area.backend().is_droppable() {
                    // A `MADV_DONTFORK` hole inside the area still splits it;
                    // `dup_mmap()` skips that range entirely.
                } else if let Some(wipe_end) =
                    Self::interval_end_covering(&self.wipe_on_fork_ranges, cursor)
                {
                    segment_end = segment_end.min(wipe_end);
                } else if let Some(next_wipe) =
                    Self::next_interval_start(&self.wipe_on_fork_ranges, cursor, area.end())
                {
                    segment_end = segment_end.min(next_wipe);
                }
                if let Some(next_dontfork) =
                    Self::next_interval_start(&self.dontfork_ranges, cursor, area.end())
                {
                    segment_end = segment_end.min(next_dontfork);
                }

                if cursor >= segment_end {
                    return Err(AxError::BadState);
                }
                count = count.checked_add(1).ok_or(AxError::NoMemory)?;
                cursor = segment_end;
            }
        }
        Ok(count)
    }

    pub(super) fn collect_relative_policy_ranges(
        ranges: &BTreeMap<VirtAddr, VirtAddr>,
        source_start: VirtAddr,
        preserve_size: usize,
    ) -> AxResult<Vec<RelativePolicyRange>> {
        let source_end = source_start
            .checked_add(preserve_size)
            .ok_or(AxError::InvalidInput)?;
        let mut relative = Vec::new();
        relative
            .try_reserve(ranges.len())
            .map_err(|_| AxError::NoMemory)?;
        for (&range_start, &range_end) in ranges.range(..source_end) {
            let start = range_start.max(source_start);
            let end = range_end.min(source_end);
            if start < end {
                relative.push(RelativePolicyRange {
                    offset: start.sub_addr(source_start),
                    size: end.sub_addr(start),
                });
            }
        }
        Ok(relative)
    }

    pub(super) fn collect_relative_policy_ranges_vec(
        ranges: &[(VirtAddr, VirtAddr)],
        source_start: VirtAddr,
        preserve_size: usize,
    ) -> AxResult<Vec<RelativePolicyRange>> {
        let source_end = source_start
            .checked_add(preserve_size)
            .ok_or(AxError::InvalidInput)?;
        let mut relative = Vec::new();
        relative
            .try_reserve(ranges.len())
            .map_err(|_| AxError::NoMemory)?;
        for &(range_start, range_end) in ranges {
            if range_start >= source_end {
                break;
            }
            let start = range_start.max(source_start);
            let end = range_end.min(source_end);
            if start < end {
                relative.push(RelativePolicyRange {
                    offset: start.sub_addr(source_start),
                    size: end.sub_addr(start),
                });
            }
        }
        Ok(relative)
    }

    pub(super) fn prepare_remap_policy(
        &mut self,
        source_start: VirtAddr,
        source_size: usize,
        destination_size: usize,
    ) -> AxResult<RemapPolicyPlan> {
        let preserve_size = source_size.min(destination_size);
        if preserve_size == 0 {
            return Err(AxError::InvalidInput);
        }
        let source_end = source_start
            .checked_add(source_size)
            .ok_or(AxError::InvalidInput)?;
        let mut wipe_on_fork = Self::collect_relative_policy_ranges(
            &self.wipe_on_fork_ranges,
            source_start,
            preserve_size,
        )?;
        let mut dontfork = Self::collect_relative_policy_ranges(
            &self.dontfork_ranges,
            source_start,
            preserve_size,
        )?;
        let mut dontdump = Self::collect_relative_policy_ranges_vec(
            &self.dontdump_ranges,
            source_start,
            preserve_size,
        )?;
        let mut locked =
            Self::collect_relative_policy_ranges(&self.locked_ranges, source_start, preserve_size)?;

        let growth = destination_size.saturating_sub(source_size);
        if growth != 0 {
            let last_source_byte = source_end - 1;
            if Self::interval_end_covering(&self.wipe_on_fork_ranges, last_source_byte).is_some() {
                wipe_on_fork.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                wipe_on_fork.push(RelativePolicyRange {
                    offset: source_size,
                    size: growth,
                });
            }
            if Self::interval_end_covering(&self.dontfork_ranges, last_source_byte).is_some() {
                dontfork.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                dontfork.push(RelativePolicyRange {
                    offset: source_size,
                    size: growth,
                });
            }
            if Self::interval_vec_end_covering(&self.dontdump_ranges, last_source_byte).is_some() {
                dontdump.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                dontdump.push(RelativePolicyRange {
                    offset: source_size,
                    size: growth,
                });
            }
            if self.range_is_fully_locked(source_start, source_size) {
                locked.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                locked.push(RelativePolicyRange {
                    offset: source_size,
                    size: growth,
                });
            }
        }

        // Every topology transaction may split a destination interval, add
        // each relative source interval, then split the source interval when
        // a move commits. Reserve that worst case before any VMA/PTE change.
        self.dontdump_ranges
            .try_reserve(dontdump.len().saturating_add(2))
            .map_err(|_| AxError::NoMemory)?;

        Ok(RemapPolicyPlan {
            growdown: self.growdown_starts.contains(&source_start),
            wipe_on_fork,
            dontfork,
            dontdump,
            locked,
        })
    }

    pub(super) fn apply_remap_policy(
        &mut self,
        destination_start: VirtAddr,
        plan: &RemapPolicyPlan,
    ) {
        if plan.growdown {
            self.growdown_starts.insert(destination_start);
        }
        for range in &plan.wipe_on_fork {
            let start = destination_start + range.offset;
            Self::insert_interval(&mut self.wipe_on_fork_ranges, start, start + range.size);
        }
        for range in &plan.dontfork {
            let start = destination_start + range.offset;
            Self::insert_interval(&mut self.dontfork_ranges, start, start + range.size);
        }
        for range in &plan.dontdump {
            let start = destination_start + range.offset;
            Self::insert_interval_vec(&mut self.dontdump_ranges, start, start + range.size);
        }
        for range in &plan.locked {
            let start = destination_start + range.offset;
            self.insert_locked_range(start, start + range.size);
        }
    }

    pub fn set_wipe_on_fork(&mut self, start: VirtAddr, size: usize, enabled: bool) -> AxResult {
        self.validate_region(start, size)?;
        Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
        if enabled {
            Self::insert_interval(&mut self.wipe_on_fork_ranges, start, start + size);
        }
        Ok(())
    }

    pub fn set_dontfork(&mut self, start: VirtAddr, size: usize, enabled: bool) -> AxResult {
        self.validate_region(start, size)?;
        Self::clear_interval(&mut self.dontfork_ranges, start, size);
        if !enabled {
            Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
        }
        if enabled {
            Self::insert_interval(&mut self.dontfork_ranges, start, start + size);
        }
        Ok(())
    }

    pub fn set_dontdump(&mut self, start: VirtAddr, size: usize, enabled: bool) -> AxResult {
        self.validate_region(start, size)?;
        let mut next = Self::try_clone_interval_vec(&self.dontdump_ranges, 2)?;
        Self::clear_interval_vec(&mut next, start, size);
        if enabled {
            Self::insert_interval_vec(&mut next, start, start + size);
        }
        self.dontdump_ranges = next;
        Ok(())
    }

    /// Materializes the exact PT_LOAD ranges eligible for a core image.
    /// MADV_DONTDUMP may cover only part of one VMA, so filtering whole areas
    /// would either leak excluded bytes or lose adjacent dumpable bytes.
    ///
    /// `VM_DROPPABLE` excludes a whole area the same way: Linux installs
    /// `VM_DONTDUMP` with it and `MADV_DODUMP` refuses to clear the exclusion
    /// again (`mm/madvise.c:1403-1406`), so no partial-range sidecar can ever
    /// re-admit one of its bytes.
    pub(crate) fn coredump_segments(&self) -> AxResult<Vec<(VirtAddr, usize, MappingFlags)>> {
        let mut segments = Vec::new();
        segments
            .try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter().filter(|area| {
            area.flags().contains(MappingFlags::USER)
                && !area.backend().is_secret()
                && !area.backend().is_droppable()
                && !area
                    .backend()
                    .file_like_mapping()
                    .is_some_and(|mapping| mapping.excludes_fork_and_dump())
        }) {
            let mut cursor = area.start();
            while cursor < area.end() {
                if let Some(excluded_end) =
                    Self::interval_vec_end_covering(&self.dontdump_ranges, cursor)
                {
                    cursor = excluded_end.min(area.end());
                    continue;
                }
                let segment_end =
                    Self::next_interval_vec_start(&self.dontdump_ranges, cursor, area.end())
                        .unwrap_or(area.end());
                if segment_end <= cursor {
                    return Err(AxError::BadState);
                }
                segments.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                segments.push((cursor, segment_end.sub_addr(cursor), area.flags()));
                cursor = segment_end;
            }
        }
        Ok(segments)
    }

    pub(super) fn insert_locked_range(&mut self, start: VirtAddr, end: VirtAddr) {
        if start >= end {
            return;
        }

        let mut new_start = start;
        let mut new_end = end;
        let overlaps: Vec<_> = self
            .locked_ranges
            .range(..=end)
            .filter_map(|(&range_start, &range_end)| {
                (range_end >= start && range_start <= end).then_some((range_start, range_end))
            })
            .collect();
        for (range_start, range_end) in overlaps {
            self.locked_ranges.remove(&range_start);
            new_start = new_start.min(range_start);
            new_end = new_end.max(range_end);
        }
        self.locked_ranges.insert(new_start, new_end);
    }

    pub(crate) fn clear_locked_range(&mut self, start: VirtAddr, size: usize) {
        if size == 0 {
            return;
        }
        let end = start + size;
        let overlaps: Vec<_> = self
            .locked_ranges
            .range(..end)
            .filter_map(|(&range_start, &range_end)| {
                (range_end > start).then_some((range_start, range_end))
            })
            .collect();
        for (range_start, range_end) in overlaps {
            self.locked_ranges.remove(&range_start);
            if range_start < start {
                self.locked_ranges.insert(range_start, start);
            }
            if range_end > end {
                self.locked_ranges.insert(end, range_end);
            }
        }
    }

    pub fn set_locked(&mut self, start: VirtAddr, size: usize, enabled: bool) -> AxResult {
        self.validate_region(start, size)?;
        self.clear_locked_range(start, size);
        if enabled {
            self.insert_locked_range(start, start + size);
        } else {
            let range = VirtAddrRange::from_start_size(start, size);
            let secret_ranges: Vec<_> = self
                .areas_overlapping(range)
                .filter(|area| area.backend().is_secret())
                .map(|area| {
                    let range_start = area.start().max(start);
                    let range_end = area.end().min(start + size);
                    (range_start, range_end)
                })
                .collect();
            for (range_start, range_end) in secret_ranges {
                self.insert_locked_range(range_start, range_end);
            }
        }
        Ok(())
    }

    pub fn range_is_locked(&self, start: VirtAddr, size: usize) -> bool {
        if size == 0 {
            return false;
        }
        let end = start + size;
        self.locked_ranges
            .range(..end)
            .any(|(&range_start, &range_end)| range_end > start && range_start < end)
    }

    /// Classifies one PMD-sized private-anonymous range for MADV_COLLAPSE.
    ///
    /// This is intentionally a conservative VMA-side proof. Long-term COW
    /// pins retain physical frames rather than virtual ranges, so their exact
    /// intersection is checked after the source leaves are collected. The
    /// caller still must validate all 4 KiB leaves and commit the replacement
    /// atomically.
    pub(super) fn private_cow_fragments_cover(
        &self,
        start: VirtAddr,
        length: usize,
        page_size: PageSize,
    ) -> bool {
        let Some(end) = start.checked_add(length) else {
            return false;
        };
        let Some(source) = self.find_area(start) else {
            return false;
        };
        if source.start() > start
            || !source.backend().is_private_cow()
            || source.backend().page_size() != page_size
        {
            return false;
        }
        let source_backend = source.backend().clone();
        let source_flags = source.flags();
        let source_lineage = source.lineage();
        let mut cursor = start;
        while cursor < end {
            let Some(area) = self.find_area(cursor) else {
                return false;
            };
            if area.start() > cursor
                || area.end() <= cursor
                || area.lineage() != source_lineage
                || area.flags() != source_flags
                || !area.backend().is_private_cow()
                || area.backend().page_size() != page_size
                || !source_backend.same_private_cow_geometry_at(area.backend(), cursor)
            {
                return false;
            }
            cursor = area.end().min(end);
        }
        true
    }

    pub(crate) fn collapse_2m_candidate_eligible(&self, start: VirtAddr, length: usize) -> bool {
        let Some(end_raw) = start.as_usize().checked_add(length) else {
            return false;
        };
        let end = VirtAddr::from(end_raw);
        let fragmented_private_cow =
            self.private_cow_fragments_cover(start, length, PageSize::Size4K);
        let range = PageRange::new(start.as_usize(), length, PAGE_SIZE_4K).ok();
        let has_uffd_write_protect = range.is_some_and(|range| {
            self.uffd.as_ref().is_some_and(|state| {
                state
                    .registrations
                    .intersecting(self.address_space_id, range)
                    .any(|registration| {
                        registration.mode().bits() & UffdRegisterMode::WP.bits() != 0
                    })
            })
        });
        // `VM_DROPPABLE` is one of the flags khugepaged refuses to collapse
        // (`mm/khugepaged.c:715`, `mm/khugepaged.c:1700` compare it against
        // `vma->vm_flags`), so a droppable area counts as having a fork
        // policy here even though no sidecar range records it.
        let has_fork_policy = Self::interval_overlaps(&self.wipe_on_fork_ranges, start, end)
            || Self::interval_overlaps(&self.dontfork_ranges, start, end)
            || self.areas.iter().any(|area| {
                area.backend().is_droppable() && area.start() < end && area.end() > start
            });
        collapse_2m_candidate_eligible(Collapse2MCandidateFacts {
            start: start.as_usize(),
            length,
            vma_covers_range: fragmented_private_cow,
            private_cow: fragmented_private_cow,
            has_uffd_write_protect,
            has_locked_pages: self.range_is_locked(start, length),
            // Exact pin ownership is established from the PTE source frames
            // below; never reject a PMD merely because another PMD is pinned.
            has_exact_long_term_cow_pin: false,
            has_fork_policy,
        })
    }

    pub(super) fn background_thp_policy_allows(&self, start: VirtAddr) -> bool {
        if self.thp_disable_mode == ThpDisableMode::Disabled {
            return false;
        }
        let Some(end) = start.checked_add(COLLAPSE_2M_SIZE) else {
            return false;
        };
        let mut cursor = start;
        while cursor < end {
            let Some(area) = self.find_area(cursor) else {
                return false;
            };
            if area.start() > cursor || area.end() <= cursor {
                return false;
            }
            let allowed = match self.thp_disable_mode {
                ThpDisableMode::Disabled => false,
                ThpDisableMode::ExceptAdvised => area.backend().madvise_thp() == MadviseThp::Huge,
                ThpDisableMode::Enabled => area.backend().madvise_thp() != MadviseThp::NoHuge,
            };
            if !allowed {
                return false;
            }
            cursor = area.end().min(end);
        }
        true
    }

    /// Forced collapse bypasses EXCEPT_ADVISED/default policy, but Linux
    /// still rejects a completely disabled mm and every VM_NOHUGEPAGE VMA.
    pub(crate) fn forced_thp_collapse_allowed(&self, start: VirtAddr, size: usize) -> bool {
        if self.thp_disable_mode == ThpDisableMode::Disabled || size == 0 {
            return false;
        }
        let Some(end) = start.checked_add(size) else {
            return false;
        };
        let mut cursor = start;
        while cursor < end {
            let Some(area) = self.find_area(cursor) else {
                return false;
            };
            if area.start() > cursor || area.backend().madvise_thp() == MadviseThp::NoHuge {
                return false;
            }
            cursor = area.end().min(end);
        }
        true
    }

    pub(super) fn background_thp_source_is_resident_4k(&self, start: VirtAddr) -> bool {
        (0..COLLAPSE_2M_SIZE).step_by(PAGE_SIZE_4K).all(|offset| {
            self.pt
                .query(start + offset)
                .is_ok_and(|(_, _, page_size)| page_size == PageSize::Size4K)
        })
    }

    /// Scans a bounded number of PMD slots for khugepaged.  Background
    /// promotion never faults file/anonymous pages in while retaining the mm
    /// lock: only fully resident 4 KiB runs reach the existing collapse
    /// transaction. The returned cursor is None after reaching this mm's end.
    pub(crate) fn collapse_background_thp_budget(
        &mut self,
        from: VirtAddr,
        budget: usize,
    ) -> Option<VirtAddr> {
        let start = from.as_usize().max(self.base().as_usize());
        let mut candidate = start
            .checked_add(COLLAPSE_2M_SIZE - 1)
            .map(|value| value & !(COLLAPSE_2M_SIZE - 1))?;
        let end = self.base().checked_add(self.size())?;
        let mut scanned = 0usize;
        while scanned < budget {
            let candidate_end = candidate.checked_add(COLLAPSE_2M_SIZE)?;
            if VirtAddr::from(candidate_end) > end {
                return None;
            }
            let address = VirtAddr::from(candidate);
            if self.background_thp_policy_allows(address)
                && self.collapse_2m_candidate_eligible(address, COLLAPSE_2M_SIZE)
                && self.background_thp_source_is_resident_4k(address)
            {
                // A concurrent topology change cannot occur under this mm
                // lock. Resource pressure or a transient pin merely leaves
                // this PMD for a later pass.
                let _ = self.collapse_private_cow_2m(address);
            }
            scanned += 1;
            candidate = candidate_end;
        }
        (VirtAddr::from(candidate) < end).then_some(VirtAddr::from(candidate))
    }

    pub(super) fn uffd_missing_registered_at(&self, vaddr: VirtAddr) -> bool {
        let Ok(page) = PageRange::new(vaddr.as_usize(), PAGE_SIZE_4K, PAGE_SIZE_4K) else {
            return false;
        };
        self.uffd.as_ref().is_some_and(|state| {
            state
                .registrations
                .intersecting(self.address_space_id, page)
                .any(|registration| {
                    registration.mode().bits() & UffdRegisterMode::MISSING.bits() != 0
                })
        })
    }
}
