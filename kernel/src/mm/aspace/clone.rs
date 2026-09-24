//! AddrSpace: fork-time address-space cloning.

use super::*;

impl AddrSpace {
    /// Attempts to clone the current address space into a new one.
    ///
    /// This method creates a new empty address space with the same base and
    /// size, then iterates over all memory areas in the original address
    /// space to copy or share their mappings into the new one.
    pub fn try_clone(&mut self) -> AxResult<Arc<Mutex<Self>>> {
        self.try_clone_with_shared_shadow_stack(None)
    }

    /// Clones an mm while preserving one vfork-borrowed shadow stack as a
    /// shared CET mapping. Ordinary fork callers must use [`Self::try_clone`]
    /// and retain its COW isolation.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn try_clone_with_shared_shadow_stack(
        &mut self,
        borrowed_shadow_stack: Option<CetDefaultShadowStackOwner>,
    ) -> AxResult<Arc<Mutex<Self>>> {
        self.try_clone_inner(borrowed_shadow_stack)
    }

    #[cfg(not(target_arch = "x86_64"))]
    pub(super) fn try_clone_with_shared_shadow_stack(
        &mut self,
        _borrowed_shadow_stack: Option<CetDefaultShadowStackOwner>,
    ) -> AxResult<Arc<Mutex<Self>>> {
        self.try_clone_inner(None)
    }

    pub(super) fn try_clone_inner(
        &mut self,
        borrowed_shadow_stack: Option<CetDefaultShadowStackOwner>,
    ) -> AxResult<Arc<Mutex<Self>>> {
        if self.user_io_pins.has_clone_blocker() {
            return Err(AxError::ResourceBusy);
        }
        if self.fork_fragment_count()? > MAX_VMA_FRAGMENTS {
            return Err(AxError::NoMemory);
        }
        #[cfg(target_arch = "x86_64")]
        if let Some(owner) = borrowed_shadow_stack.as_ref() {
            // A vfork borrow is an exact lease from this source mm.  Validate
            // it before any COW PTE mutation: pkey_mprotect(PROT_READ) may
            // have split the VMA, but every fragment of the logical extent
            // must remain live SHSTK and no fork policy may punch a child
            // hole through the borrowed stack.
            if !self.cet_default_shadow_stacks.contains(&owner)
                || owner.extents.is_empty()
                || owner.extents.iter().any(|extent| {
                    extent.end().is_none_or(|end| {
                        !self.cet_shadow_stack_extent_covers(extent.start, extent.size)
                            || Self::interval_overlaps(&self.dontfork_ranges, extent.start, end)
                            || Self::interval_overlaps(&self.wipe_on_fork_ranges, extent.start, end)
                    })
                })
            {
                return Err(AxError::BadState);
            }
        }
        // Resolve owner-aware physical identities before any parent PTE is
        // COW-protected. Allocation failure therefore leaves fork entirely
        // unpublished and the parent untouched.
        let active_long_term_cow_frames = self.active_long_term_cow_frames()?;

        let new_aspace = Arc::new(Mutex::new(Self::new_empty(self.base(), self.size())?));
        crate::mm::register_pending_address_space(&new_aspace);
        let next_topology_generation = self.next_topology_generation()?;
        let new_aspace_clone = new_aspace.clone();
        let wipe_on_fork_ranges = self.wipe_on_fork_ranges.clone();
        let dontfork_ranges = self.dontfork_ranges.clone();
        let dontdump_ranges =
            Self::try_clone_interval_vec(&self.dontdump_ranges, self.dontfork_ranges.len())?;
        let madvise_guard_ranges = self.madvise_guard_ranges.clone();
        let madvise_hwpoison_ranges = self.madvise_hwpoison_ranges.clone();

        // Reserve every child identity before clone_map can COW-protect a
        // parent PTE. Fork does not change the parent's VMA contract, so its
        // per-lineage generation remains stable; the legacy topology
        // generation below remains the conservative PTE/COW admission fence.
        let mut child_parent_lineages = Vec::new();
        child_parent_lineages
            .try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            if area
                .backend()
                .file_like_mapping()
                .is_some_and(|mapping| mapping.excludes_fork_and_dump())
            {
                continue;
            }
            let mut cursor = area.start();
            let mut has_child_segment = false;
            while cursor < area.end() {
                if let Some(dontfork_end) = Self::interval_end_covering(&dontfork_ranges, cursor) {
                    cursor = dontfork_end.min(area.end());
                } else {
                    has_child_segment = true;
                    break;
                }
            }
            if has_child_segment {
                child_parent_lineages.push(area.lineage());
            }
        }
        child_parent_lineages.sort_unstable();
        child_parent_lineages.dedup();

        let mut guard = new_aspace.lock();
        // Bind every shared source backing before clone_map can make a parent
        // COW-visible change.  The child is not published yet, so a later
        // failure drops these leases with the unpublished child; a successful
        // final sync removes any DONTFORK-only provisional bindings.
        let mut fork_shared_keys = Vec::new();
        fork_shared_keys
            .try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            if area
                .backend()
                .file_like_mapping()
                .is_some_and(|mapping| mapping.excludes_fork_and_dump())
            {
                continue;
            }
            if let Some(key) = area.backend().shared_backing_key() {
                fork_shared_keys.push(key);
            }
        }
        fork_shared_keys.sort_unstable();
        fork_shared_keys.dedup();
        // The child has no VMA yet.  Reserve every potential shared backing
        // as pending so a cross-mm folio transaction cannot snapshot between
        // clone_map publication and the final child registry commit.
        let mut fork_pending_aliases = Vec::new();
        fork_pending_aliases
            .try_reserve_exact(fork_shared_keys.len())
            .map_err(|_| AxError::NoMemory)?;
        for key in fork_shared_keys {
            fork_pending_aliases.push(PendingAliasLease::try_prepare(
                key,
                &new_aspace_clone,
                guard.address_space_id,
            )?);
        }
        // Linux carries private membarrier registrations across an ordinary
        // fork, but the child starts with no CPUs resident and a fresh
        // barrier generation. CLONE_VM shares the address space (and hence
        // this state) through the existing Arc path instead.
        guard.tlb = self.tlb.fork_clone()?;
        guard.thp_disable_mode = self.thp_disable_mode;
        if let Some(old) = self.tlb.snapshot_ldt() {
            guard.tlb.replace_ldt(Some(
                Arc::try_new(old.copy()?).map_err(|_| AxError::NoMemory)?,
            ));
        }
        guard.growdown_starts = self.growdown_starts.clone();
        guard.dontdump_ranges = dontdump_ranges;
        guard.madvise_guard_ranges = madvise_guard_ranges;
        guard.madvise_hwpoison_ranges = madvise_hwpoison_ranges;
        let excluded_area_count = self
            .areas
            .iter()
            .filter(|area| {
                area.backend()
                    .file_like_mapping()
                    .is_some_and(|mapping| mapping.excludes_fork_and_dump())
            })
            .count();
        if excluded_area_count != 0 {
            guard
                .dontdump_ranges
                .try_reserve(excluded_area_count)
                .map_err(|_| AxError::NoMemory)?;
            // An intrinsically excluded mapping has no child VMA.  Remove any
            // inherited DONTDUMP interval over it so a later child mmap at
            // the same address does not inherit policy from the absent VMA.
            for area in self.areas.iter() {
                if area
                    .backend()
                    .file_like_mapping()
                    .is_some_and(|mapping| mapping.excludes_fork_and_dump())
                {
                    Self::clear_interval(
                        &mut guard.madvise_guard_ranges,
                        area.start(),
                        area.size(),
                    );
                    Self::clear_interval(
                        &mut guard.madvise_hwpoison_ranges,
                        area.start(),
                        area.size(),
                    );
                    Self::clear_interval_vec(&mut guard.dontdump_ranges, area.start(), area.size());
                }
            }
        }
        // DONTFORK holes have no child VMA.  Do not leave an advisory guard
        // sidecar there: a later child mmap at that address must not inherit
        // a parent's fault policy.
        for (&start, &end) in &dontfork_ranges {
            Self::clear_interval(&mut guard.madvise_guard_ranges, start, end.sub_addr(start));
            Self::clear_interval(
                &mut guard.madvise_hwpoison_ranges,
                start,
                end.sub_addr(start),
            );
            Self::clear_interval_vec(&mut guard.dontdump_ranges, start, end.sub_addr(start));
        }
        let mut child_lineages = Vec::new();
        child_lineages
            .try_reserve(child_parent_lineages.len())
            .map_err(|_| AxError::NoMemory)?;
        for parent_lineage in child_parent_lineages {
            let child_lineage = guard.prepare_fresh_mapping_lineage()?;
            child_lineages.push((parent_lineage, child_lineage));
        }

        self.commit_topology_generation(next_topology_generation);

        let mut self_modify = self.pt.cursor();
        for area in self.areas.iter() {
            if area
                .backend()
                .file_like_mapping()
                .is_some_and(|mapping| mapping.excludes_fork_and_dump())
            {
                continue;
            }
            let child_lineage = child_lineages
                .binary_search_by_key(&area.lineage(), |(parent, _)| *parent)
                .ok()
                .map(|index| child_lineages[index].1);
            let page_size = area.backend().page_size();
            let mut cursor = area.start();
            while cursor < area.end() {
                if let Some(dontfork_end) = Self::interval_end_covering(&dontfork_ranges, cursor) {
                    cursor = dontfork_end.min(area.end());
                    continue;
                }

                // `MAP_DROPPABLE` is derived here rather than recorded in the
                // wipe sidecar: Linux reaches `dup_mmap()`'s wipe branch
                // through the `VM_WIPEONFORK` flag it installed with
                // `VM_DROPPABLE` (`mm/mmap.c:533`, `mm/mmap.c:1796-1801`), so
                // the whole droppable area starts absent in the child.  A
                // `MADV_DONTFORK` hole still clips the fragment because
                // `dup_mmap()` skips such ranges outright.
                let derived_wipe_end = if area.backend().is_droppable() {
                    Some(
                        Self::next_interval_start(&dontfork_ranges, cursor, area.end())
                            .unwrap_or_else(|| area.end()),
                    )
                } else {
                    None
                };
                if let Some(wipe_end) = derived_wipe_end
                    .or_else(|| Self::interval_end_covering(&wipe_on_fork_ranges, cursor))
                {
                    let segment_end = wipe_end.min(area.end());
                    let wipe_size = segment_end.sub_addr(cursor);
                    debug_assert!(page_size.is_aligned(wipe_size));
                    let child_backend = wipe_on_fork_backend(
                        cursor,
                        page_size,
                        area.backend().is_sealed(),
                        area.backend().is_droppable(),
                    );
                    let new_area = MemoryArea::new_with_lineage(
                        cursor,
                        wipe_size,
                        area.flags(),
                        child_backend,
                        child_lineage.expect("included parent lineage was not prepared"),
                    );
                    let aspace = guard.deref_mut();
                    aspace.areas.map_with_limit(
                        new_area,
                        &mut aspace.pt,
                        false,
                        MAX_VMA_FRAGMENTS,
                    )?;
                    Self::insert_interval(&mut aspace.wipe_on_fork_ranges, cursor, segment_end);
                    // WIPEONFORK replaces the parent backing with fresh zero
                    // pages; fault poison and lazyfree identity belong to the
                    // retired parent leaves and must not follow it.
                    Self::clear_interval(&mut aspace.madvise_guard_ranges, cursor, wipe_size);
                    Self::clear_interval(&mut aspace.madvise_hwpoison_ranges, cursor, wipe_size);
                    aspace
                        .madvise_free_pages
                        .retain(|&page, _| page < cursor || page >= segment_end);
                    cursor = segment_end;
                    continue;
                }

                let mut segment_end = area.end();
                if let Some(next_start) =
                    Self::next_interval_start(&dontfork_ranges, cursor, area.end())
                {
                    segment_end = segment_end.min(next_start);
                }
                if let Some(next_start) =
                    Self::next_interval_start(&wipe_on_fork_ranges, cursor, area.end())
                {
                    segment_end = segment_end.min(next_start);
                }

                if cursor < segment_end {
                    let segment_size = segment_end.sub_addr(cursor);
                    let share_shadow_stack = borrowed_shadow_stack.as_ref().is_some_and(|owner| {
                        // Each whole SHSTK fragment inside the already
                        // validated logical lease keeps the parent's CET
                        // leaves.  Do not share a merely overlapping VMA:
                        // that could cover a hole, an adjacent explicit
                        // SHSTK mapping, or another owner's extent.
                        area.flags().contains(MappingFlags::SHADOW_STACK)
                            && owner.extents.iter().any(|extent| {
                                extent
                                    .end()
                                    .is_some_and(|end| extent.start <= cursor && segment_end <= end)
                            })
                    });
                    let new_backend = {
                        let mut new_modify = guard.pt.cursor_no_flush();
                        area.backend().clone_map(
                            VirtAddrRange::from_start_size(cursor, segment_size),
                            area.flags(),
                            &mut self_modify,
                            &mut new_modify,
                            &new_aspace_clone,
                            &active_long_term_cow_frames,
                            share_shadow_stack,
                        )?
                    };
                    // Fork keeps the segment at the same virtual address. In
                    // particular, a suffix after MADV_DONTFORK must retain the
                    // original backend origin and file-offset relation.
                    let new_area = MemoryArea::new_with_lineage(
                        cursor,
                        segment_size,
                        area.flags(),
                        new_backend,
                        child_lineage.expect("included parent lineage was not prepared"),
                    );
                    let aspace = guard.deref_mut();
                    aspace.areas.map_with_limit(
                        new_area,
                        &mut aspace.pt,
                        false,
                        MAX_VMA_FRAGMENTS,
                    )?;
                    if Self::interval_overlaps(&wipe_on_fork_ranges, cursor, segment_end) {
                        Self::insert_interval(&mut aspace.wipe_on_fork_ranges, cursor, segment_end);
                    }
                    cursor = segment_end;
                } else {
                    cursor += page_size as usize;
                }
            }
        }
        // Present private leaves were handled by `clone_map`; copy software
        // swap PTEs separately and take one slot reference for the child.
        // DONTFORK drops the child ownership and WIPEONFORK intentionally
        // starts absent/zero-filled.
        for (page, entry) in &self.swapped {
            if Self::interval_end_covering(&dontfork_ranges, *page).is_some()
                || Self::interval_end_covering(&wipe_on_fork_ranges, *page).is_some()
                || self.areas.find(*page).is_some_and(|area| {
                    area.backend()
                        .file_like_mapping()
                        .is_some_and(|mapping| mapping.excludes_fork_and_dump())
                })
            {
                continue;
            }
            crate::mm::retain(*entry)?;
            guard.swapped.insert(*page, *entry);
        }
        // Secret VMAs are unconditionally mlocked.  Reconstruct this child
        // sidecar from VMAs that were actually cloned, rather than copying
        // the parent's ranges: MADV_DONTFORK holes have no child mapping and
        // ordinary mlock state is not inherited by fork.
        let child_secret_ranges: Vec<_> = guard
            .areas
            .iter()
            .filter(|area| area.backend().is_secret())
            .map(|area| (area.start(), area.end()))
            .collect();
        for (start, end) in child_secret_ranges {
            guard.insert_locked_range(start, end);
        }
        guard.refresh_growdown_starts();
        for pending in fork_pending_aliases.drain(..) {
            guard.commit_shared_alias_binding(pending);
        }
        guard.sync_shared_alias_bindings(&new_aspace_clone)?;
        // A forked mm starts with the child's currently resident pages as its
        // initial peak; unlike CLONE_VM this is a distinct address space.
        guard.publish_resident_highwater();
        debug_assert!(
            guard.areas.iter().all(|area| mapping_identity(
                &guard.mapping_identities,
                area.lineage()
            )
            .is_ok())
        );
        drop(self_modify);
        drop(self.synchronize_tlb_after_mutation());
        drop(guard);

        Ok(new_aspace)
    }

    /// Returns an iterator over the memory areas.
    ///
    /// This is required for `procfs` to generate `/proc/pid/maps`.
    /// Exposing internal state for system introspection is a standard practice.
    pub fn areas(&self) -> impl Iterator<Item = &MemoryArea<Backend>> {
        self.areas.iter()
    }
}
