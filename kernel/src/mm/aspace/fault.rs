//! AddrSpace: protection changes, page faults, reclaim and swap.

use super::*;

impl AddrSpace {
    pub(super) fn prepare_protect_ranges(
        &mut self,
        start: VirtAddr,
        size: usize,
        ranges: Vec<PreparedProtectRange<VirtAddr, MappingFlags>>,
    ) -> AxResult<PreparedProtect<'_>> {
        self.validate_region(start, size)?;
        if size == 0 {
            return Err(AxError::InvalidInput);
        }
        self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Protect)?;
        for range in &ranges {
            self.check_protect_range(range.start, range.end.sub_addr(range.start), range.flags)?;
        }
        let next_topology_generation = self.next_topology_generation()?;
        let mapping_mutations = prepare_mapping_generation_advances_for_range(
            &self.areas,
            &self.mapping_identities,
            start,
            size,
        )?;

        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let protect = VirtAddrRange::new(start, end);
        let protect_range =
            PageRange::new(start.as_usize(), size, PAGE_SIZE_4K).map_err(mm_error)?;

        let AddrSpace {
            address_space_id,
            areas,
            mapping_identities,
            growdown_starts,
            uffd,
            topology_generation,
            tlb,
            pt,
            ..
        } = self;
        let address_space_id = *address_space_id;
        let uffd_mutation = if let Some(state) = uffd.as_deref_mut() {
            let plan = state.preflight_protect(0, protect_range, |registration, fragment| {
                Self::projected_uffd_protect_snapshot(
                    address_space_id,
                    areas,
                    mapping_identities,
                    protect,
                    ranges.as_slice(),
                    registration,
                    fragment,
                )
            })?;
            match plan {
                OptionalUffdPlan::Noop => None,
                OptionalUffdPlan::Armed(_) => Some(PreparedUffdMutation::new(state, plan)),
            }
        } else {
            None
        };

        let transaction = PreparedAreaProtect {
            areas,
            page_table: pt,
            start,
            end,
            ranges,
            max_areas: MAX_VMA_FRAGMENTS,
        };
        let synchronize_instruction_stream = transaction
            .segments()
            .any(|(area, _, _, flags)| adds_execute_permission(area.flags(), flags));

        Ok(PreparedProtect {
            transaction,
            growdown_starts,
            topology_generation,
            next_topology_generation,
            tlb,
            mapping_identities,
            mapping_mutations,
            uffd_mutation,
            synchronize_instruction_stream,
        })
    }

    pub(super) fn check_protect_range(
        &self,
        mut start: VirtAddr,
        size: usize,
        flags: MappingFlags,
    ) -> AxResult {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;

        while start < end {
            let Some(area) = self.areas.find(start) else {
                ax_bail!(NoMemory);
            };
            if area.start() > start {
                ax_bail!(NoMemory);
            }
            if area.backend().special_mapping_token().is_some() {
                // The special mapping's RX contract and token are immutable;
                // even a nominal no-op must not split/rekey it. Callers retain
                // the preceding Linux mprotect prefix, then fail here.
                ax_bail!(OperationNotPermitted);
            }
            area.backend().check_protect_flags(flags)?;
            start = area.end().min(end);
        }

        Ok(())
    }

    /// Preflight used by pkey_mprotect before it prepares huge-leaf demotion
    /// or PTE changes. A special mapping is never a legal permission target.
    pub(crate) fn reject_special_mapping_mutation(
        &self,
        mut start: VirtAddr,
        size: usize,
    ) -> AxResult {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        while start < end {
            let Some(area) = self.areas.find(start) else {
                ax_bail!(NoMemory);
            };
            if area.start() > start {
                ax_bail!(NoMemory);
            }
            if area.backend().special_mapping_token().is_some() {
                ax_bail!(OperationNotPermitted);
            }
            start = area.end().min(end);
        }
        Ok(())
    }

    /// Fixed mremap destinations may legitimately contain holes.  Scan only
    /// existing overlapping VMAs while retaining the same special-mapping
    /// rejection rule as a fully mapped source range.
    pub(crate) fn reject_special_mapping_overlap(&self, start: VirtAddr, size: usize) -> AxResult {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        for area in self.areas.iter_overlapping(VirtAddrRange::new(start, end)) {
            if area.backend().special_mapping_token().is_some() {
                ax_bail!(OperationNotPermitted);
            }
        }
        Ok(())
    }

    /// Removes all mappings and starts a fresh identity generation for exec.
    pub fn clear(&mut self) -> AxResult {
        if self.user_io_pins.progress().total() != 0 {
            return Err(AxError::ResourceBusy);
        }
        // Image reset is currently used only on a fresh address space. Do not
        // silently recycle an mm which still owns UFFD registrations,
        // terminal results, or waiter credits; that lifecycle needs an
        // explicit lock-external detach receipt.
        if self.uffd.is_some() {
            return Err(AxError::ResourceBusy);
        }
        // Reserve the replacement identity before destroying the current
        // image. A sequence-exhaustion failure therefore leaves it untouched.
        let new_policy = new_user_io_policy()?;
        crate::uprobe::invalidate_xol_range_locked(self, self.base(), self.size());
        self.clear_areas_with_tlb_grace()?;
        // Keep the registry exact even when the Arc survives an exec image
        // replacement.  Otherwise a later cross-mm transaction needlessly
        // locks this unrelated mm (and an identity reset makes snapshots
        // impossible to revalidate).
        self.alias_bindings.clear();
        drop(core::mem::take(&mut self.mapping_identities));
        self.growdown_starts.clear();
        self.madvise_guard_ranges.clear();
        self.madvise_hwpoison_ranges.clear();
        self.madvise_free_pages.clear();
        self.wipe_on_fork_ranges.clear();
        self.dontfork_ranges.clear();
        self.dontdump_ranges.clear();
        self.locked_ranges.clear();
        debug_assert!(self.active_long_term_cow_pins.is_empty());
        self.user_io_pins.begin_teardown().map_err(mm_error)?;
        self.user_io_pins.finish_teardown().map_err(mm_error)?;
        (
            self.address_space_id,
            self.topology_mapping_id,
            self.topology_generation,
            self.user_io_pins,
        ) = new_policy;
        Ok(())
    }

    pub(super) fn try_handle_growdown_fault(
        &mut self,
        vaddr: VirtAddr,
        access_flags: PageFaultFlags,
        user_sp: Option<VirtAddr>,
    ) -> PageFaultResult {
        let Some(user_sp) = user_sp else {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        };

        // Linux grows MAP_GROWSDOWN mappings when the fault lands on the guard
        // page immediately below the current lowest mapped page and SP is still
        // within that guard page.
        let Some((current_start, current_end, fault_page, page_size, flags, lineage, backend)) =
            self.growdown_starts
                .iter()
                .copied()
                .find_map(|current_start| {
                    let area = self.areas.find(current_start)?;
                    if area.start() != current_start {
                        return None;
                    }
                    let page_size = area.backend().page_size();
                    let fault_page = vaddr.align_down(page_size);
                    if fault_page.checked_add(page_size as usize)? != current_start {
                        return None;
                    }
                    // Linux permits a stack fault below RSP by a bounded window;
                    // compilers commonly probe/allocate before adjusting RSP, and
                    // RSP may still point inside the old VMA.  Keep the window
                    // bounded so an arbitrary low-address fault cannot grow it.
                    const STACK_FAULT_SP_WINDOW: usize = 64 * 1024;
                    if !(user_sp >= vaddr
                        && user_sp.sub_addr(vaddr) <= STACK_FAULT_SP_WINDOW.saturating_add(32))
                    {
                        return None;
                    }
                    match area.backend() {
                        Backend::Cow(_) => Some((
                            current_start,
                            area.end(),
                            fault_page,
                            page_size,
                            area.flags(),
                            area.lineage(),
                            area.backend().clone(),
                        )),
                        Backend::Linear(_) | Backend::Shared(_) | Backend::File(_) => None,
                    }
                })
        else {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        };
        if !flags.contains(access_flags) {
            return PageFaultResult::Failed(PageFaultFailure::AccessDenied);
        }

        let Some(gap_start) =
            current_start.checked_sub(page_size as usize * Self::STACK_GUARD_GAP_PAGES)
        else {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        };
        if self.areas.overlaps(VirtAddrRange::from_start_size(
            gap_start,
            current_start.sub_addr(gap_start),
        )) {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        }
        // MAP_GROWSDOWN is automatic address-space growth too: do not let a
        // fault turn a CET stack's policy-only lower guard into an ordinary
        // stack page merely because no VMA occupies that page.
        let grow_range = VirtAddrRange::from_start_size(fault_page, page_size as usize);
        if self.find_free_area_avoiding_shadow_stack_guards(
            fault_page,
            page_size as usize,
            grow_range,
            page_size as usize,
        ) != Some(fault_page)
        {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        }

        // Linux's automatic stack expansion accounts the extra VMA page
        // before checking RLIMIT_STACK or attempting population.  This read
        // is made at the fault edge so a concurrent prlimit applies to the
        // next growth, while this address-space lock makes the total+growth
        // comparison atomic with VMA publication.
        let rlimit_as_allows_growth = axtask::current_may_uninit()
            .and_then(|task| {
                task.try_as_thread().map(|thread| {
                    super::super::check_rlimit_as_growth(
                        &thread.proc_data,
                        self,
                        page_size as usize,
                    )
                    .is_ok()
                })
            })
            .unwrap_or(false);
        if !rlimit_as_allows_growth {
            return PageFaultResult::Failed(PageFaultFailure::OutOfMemory);
        }

        // MAP_GROWSDOWN is constrained by the task's current stack rlimit,
        // not merely by free virtual address space.  Read it at the fault
        // edge so a concurrent prlimit/setrlimit takes effect for the next
        // expansion; infinity retains the architecture's normal VA limit.
        let stack_limit = axtask::current_may_uninit()
            .map(|task| {
                task.try_as_thread()
                    .map(|thread| thread.proc_data.rlim.read()[RLIMIT_STACK].current)
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        if stack_limit != RLIM_INFINITY as i64 as u64
            && current_end.sub_addr(fault_page) > stack_limit as usize
        {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        }

        let locked = self.range_is_fully_locked(current_start, page_size as usize);
        if let Err(error) = self.extend_mapping_head_with_existing_lineage(
            current_start,
            fault_page,
            page_size as usize,
            flags,
            backend,
            locked,
            lineage,
        ) {
            if error.published() {
                self.move_growdown_start(current_start, fault_page);
                // Mapping and UFFD sidecar authority are now visible. Return
                // to the user-fault dispatcher instead of populating under
                // this recursive lock-held path; the retried instruction must
                // pass through delegated-fault admission for the new page.
                return PageFaultResult::Handled;
            }
            let err = error.into_error();
            debug!(
                "Failed to extend MAP_GROWSDOWN mapping from {current_start:?} to {fault_page:?}: \
                 {err}"
            );
            return if err.canonicalize() == AxError::NoMemory {
                PageFaultResult::Failed(PageFaultFailure::OutOfMemory)
            } else {
                PageFaultResult::Failed(PageFaultFailure::AddressNotMapped)
            };
        }
        self.move_growdown_start(current_start, fault_page);
        // Growth is one committed transition. A second hardware fault either
        // delegates the inherited UFFD MISSING registration or performs the
        // ordinary population path. `Handled` already means retry the same
        // userspace instruction at the trap boundary.
        PageFaultResult::Handled
    }

    /// Checks whether an access to the specified memory region is valid.
    ///
    /// Returns `true` if the memory region given by `range` is all mapped and
    /// has proper permission flags (i.e. containing `access_flags`).
    pub fn can_access_range(
        &self,
        start: VirtAddr,
        size: usize,
        access_flags: MappingFlags,
    ) -> bool {
        let Some(mut range) = VirtAddrRange::try_from_start_size(start, size) else {
            return false;
        };
        for area in self.areas.iter() {
            if area.end() <= range.start {
                continue;
            }
            if area.start() > range.start {
                return false;
            }

            // This area overlaps with the memory region
            if !area.flags().contains(access_flags) {
                return false;
            }

            range.start = area.end();
            if range.is_empty() {
                return true;
            }
        }

        false
    }

    /// Swaps out every exclusively-owned 4 KiB anonymous leaf in
    /// `[start, start + size)`, returning how many were moved.
    ///
    /// This is the anonymous half of Linux `MADV_PAGEOUT` /
    /// `process_madvise(MADV_PAGEOUT)`: `mm/madvise.c:madvise_pageout_pte_range()`
    /// calls `pageout()` on each eligible leaf and the syscall still reports
    /// success for every leaf it could not reclaim.  A leaf is eligible here
    /// only when its backend owns the frame exclusively (see
    /// `Backend::swap_reclaimable`), which is the local equivalent of Linux's
    /// `folio_mapcount() == 1` isolated-LRU admission; anything else is left
    /// resident rather than being discarded.
    ///
    /// A `MAP_DROPPABLE` leaf is dropped instead of written out, so it needs
    /// no active swap area and never becomes a swap entry.
    pub(crate) fn reclaim_anonymous_pages_in_range(
        &mut self,
        start: VirtAddr,
        size: usize,
    ) -> AxResult<usize> {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let mut page = VirtAddr::from(start.as_usize().next_multiple_of(PAGE_SIZE_4K));
        let mut reclaimed = 0;
        while page < end {
            match self.reclaim_anonymous_page_at(page)? {
                AnonymousReclaim::Reclaimed | AnonymousReclaim::Dropped => reclaimed += 1,
                AnonymousReclaim::NotEligible => {}
                // Without an active swap area Linux's `pageout()` finds no
                // swap slot, keeps every leaf resident, and still returns
                // success; stopping here avoids walking a large range for
                // nothing.  Droppable leaves never reach this arm: they are
                // discarded without allocating a slot.
                AnonymousReclaim::NoSwapArea => break,
            }
            page += PAGE_SIZE_4K;
        }
        Ok(reclaimed)
    }

    /// Reclaims one exclusively-owned 4 KiB anonymous leaf.  The present PTE
    /// is first invalidated and globally quiesced, so a concurrent CPU cannot
    /// modify bytes while they are copied to swap.  A failed pageout restores
    /// the original leaf before returning.
    ///
    /// A `MAP_DROPPABLE` leaf takes Linux's discard arm instead: it is freed
    /// without being written to swap.
    ///
    /// The pageout I/O itself runs under this address space's lock on purpose.
    /// Linux holds the mmap read lock across `pageout()` for the same reason:
    /// the swap entry must be recorded before the lock is dropped, because a
    /// fault on the just-invalidated address would otherwise repopulate it with
    /// a fresh zero page and silently lose the saved bytes.
    pub(super) fn reclaim_anonymous_page_at(
        &mut self,
        page: VirtAddr,
    ) -> AxResult<AnonymousReclaim> {
        let (backend, area_page_size, droppable) = match self.areas.find(page) {
            Some(area) => (
                area.backend().clone(),
                area.backend().page_size(),
                area.backend().is_droppable(),
            ),
            None => return Ok(AnonymousReclaim::NotEligible),
        };
        if area_page_size != PageSize::Size4K {
            return Ok(AnonymousReclaim::NotEligible);
        }
        let Ok((paddr, _, PageSize::Size4K)) = self.pt.query(page) else {
            return Ok(AnonymousReclaim::NotEligible);
        };
        if !backend.swap_reclaimable(paddr) {
            return Ok(AnonymousReclaim::NotEligible);
        }
        // A pinned frame may still be modified by in-flight DMA.  Deferring
        // allocator reuse is insufficient: the persisted image would already
        // be stale, so reclaim must reject the victim before pageout.
        if self
            .check_no_user_io_pin_overlap(page, PAGE_SIZE_4K, InvalidationReason::Discard)
            .is_err()
        {
            return Ok(AnonymousReclaim::NotEligible);
        }
        let (_, leaf_flags, leaf_size) = self.pt.cursor().unmap(page).map_err(AxError::from)?;
        if leaf_size != PageSize::Size4K {
            return Err(AxError::BadState);
        }
        drop(self.synchronize_tlb_after_mutation());
        if droppable {
            // Linux never marks a droppable folio swapbacked
            // (`mm/rmap.c:1652-1655` records that the flag is the difference
            // between `MADV_FREE` and `MADV_DROPPABLE` pages), so
            // `try_to_unmap_one()` reaches its `discard` arm
            // (`mm/rmap.c:2287`) instead of building a swap entry: the PTE is
            // cleared and the folio is freed outright.  The bytes are gone and
            // the next fault reads a fresh zero page, which is exactly what
            // `MAP_DROPPABLE` promises — and why this arm needs no swap area
            // while the pageout arm does.
            let grace = self.synchronize_tlb_after_mutation();
            backend.release_swapped_frame(paddr);
            drop(grace);
            return Ok(AnonymousReclaim::Dropped);
        }
        // SAFETY: the leaf was unmapped and the TLB synchronized above, so no user mapping can
        // write `paddr` while this 4 KiB frame is read for pageout.
        let bytes =
            unsafe { core::slice::from_raw_parts(phys_to_virt(paddr).as_ptr(), PAGE_SIZE_4K) };
        let entry = match crate::mm::pageout(bytes) {
            Ok(entry) => entry,
            Err(error) => {
                self.pt
                    .cursor()
                    .map(page, paddr, PageSize::Size4K, leaf_flags)
                    .map_err(AxError::from)?;
                drop(self.synchronize_tlb_after_mutation());
                return Ok(match error.canonicalize() {
                    // `swap::allocate_slot()` reports both "no active swap
                    // area" and "every slot is taken" as ENOSPC, which is the
                    // same admission failure Linux's `pageout()` sees when the
                    // LRU cannot find a swap slot.
                    AxError::NoMemory | AxError::StorageFull => AnonymousReclaim::NoSwapArea,
                    // Any other pageout failure leaves the leaf resident, which
                    // is what Linux reports as an unreclaimable page.
                    _ => AnonymousReclaim::NotEligible,
                });
            }
        };
        self.swapped.insert(page, entry);
        let grace = self.synchronize_tlb_after_mutation();
        backend.release_swapped_frame(paddr);
        drop(grace);
        Ok(AnonymousReclaim::Reclaimed)
    }

    /// Captures and pins all target entries while the caller holds this mm
    /// lock. Allocation and I/O are deliberately deferred to `prepare`.
    pub(crate) fn snapshot_swapoff_area(&self, area: u16) -> AxResult<Vec<SwapoffPage>> {
        let count = self
            .swapped
            .values()
            .filter(|entry| entry.area() == area)
            .count();
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        for (page, entry) in self
            .swapped
            .iter()
            .filter(|(_, entry)| entry.area() == area)
        {
            let mapping = self.areas.find(*page).ok_or(AxError::BadState)?;
            if !mapping.backend().supports_uffd_missing_resolver() {
                return Err(AxError::BadState);
            }
            crate::mm::retain(*entry)?;
            pages.push(SwapoffPage {
                page: *page,
                entry: *entry,
            });
        }
        Ok(pages)
    }

    /// Validates the complete preflight set while all live MM locks are held.
    /// No page-table state changes here, allowing the caller to abandon every
    /// prepared page with zero migration on any mismatch.
    pub(crate) fn validate_swapoff_pages(&self, pages: &[PreparedSwapoffPage]) -> AxResult<()> {
        for page in pages {
            match self.swapped.get(&page.page) {
                Some(entry) if *entry == page.entry => {
                    let mapping = self.areas.find(page.page).ok_or(AxError::BadState)?;
                    if !mapping.backend().supports_uffd_missing_resolver() {
                        return Err(AxError::BadState);
                    }
                }
                // A fault may have restored the entry after snapshot. That
                // already satisfies swapoff and only leaves our temporary pin.
                None => {}
                Some(_) => return Err(AxError::BadState),
            }
        }
        Ok(())
    }

    /// Infallible half of the global swapoff transaction. Validation and all
    /// allocation precede this call while all MM locks remain held.
    pub(crate) fn commit_swapoff_pages(&mut self, pages: &mut [PreparedSwapoffPage]) {
        for page in pages {
            if self.swapped.get(&page.page) != Some(&page.entry) {
                continue;
            }
            let mapping = self
                .areas
                .find(page.page)
                .expect("validated swapoff VMA vanished");
            let backend = mapping.backend().clone();
            let flags = mapping.flags();
            backend
                .publish_prepared_cow_page(page.page, flags, &mut self.pt, &mut page.prepared)
                .expect("validated swapoff publication consumed preallocated resources");
            self.swapped.remove(&page.page);
            crate::mm::release(page.entry).expect("validated swapoff entry disappeared");
            self.publish_resident_highwater();
        }
    }

    pub(super) fn release_swapped_range(&mut self, start: VirtAddr, size: usize) {
        let end = start + size;
        while let Some((page, entry)) = self
            .swapped
            .range(start..end)
            .next()
            .map(|(page, entry)| (*page, *entry))
        {
            self.swapped.remove(&page);
            let _ = crate::mm::release(entry);
        }
    }

    /// Stages non-present anonymous software PTEs at an mremap destination.
    /// This always takes a destination reference.  A moving transaction keeps
    /// its source reference until its normal source-unmap commit, making a
    /// failed staged move rollback-safe without a special restore path.
    pub(crate) fn relocate_swapped_entries(
        &mut self,
        source: VirtAddr,
        destination: VirtAddr,
        size: usize,
    ) -> AxResult {
        let end = source + size;
        let entries: Vec<_> = self
            .swapped
            .range(source..end)
            .map(|(page, entry)| (*page, *entry))
            .collect();
        for (page, entry) in entries {
            let destination_page = destination + page.sub_addr(source);
            crate::mm::retain(entry)?;
            if let Some(displaced) = self.swapped.insert(destination_page, entry) {
                // A destination is required to be empty by the remap
                // transaction.  Treat a violation as ownership corruption
                // rather than silently releasing an unrelated swap PTE.
                let _ = crate::mm::release(entry);
                self.swapped.insert(destination_page, displaced);
                return Err(AxError::AlreadyExists);
            }
        }
        Ok(())
    }

    /// Checks whether this trap still names a real missing/unsatisfied leaf.
    /// The eventual minor/major classification is deliberately deferred until
    /// backend population completes, when the task's backing-read counter can
    /// prove that storage I/O actually occurred.
    pub(crate) fn fault_needs_accounting(
        &self,
        vaddr: VirtAddr,
        access_flags: PageFaultFlags,
    ) -> bool {
        let Some(area) = self.areas.find(vaddr) else {
            return false;
        };
        if !area.flags().contains(access_flags) {
            return false;
        }
        if self.swapped.contains_key(&vaddr.align_down(PAGE_SIZE_4K)) {
            return true;
        }
        match self.pt.query(vaddr.align_down(PAGE_SIZE_4K)) {
            Ok((_paddr, page_flags, _page_size)) => {
                if present_leaf_satisfies_fault(page_flags, access_flags) {
                    return false;
                }
                true
            }
            Err(PagingError::NotMapped) => true,
            Err(_) => false,
        }
    }

    /// Retains a file cache for the fault retry path without starting reclaim
    /// while `self` is locked.  The returned handle is revalidated by the
    /// normal fault loop after its transaction finishes.
    pub(crate) fn file_cache_reclaim_for_fault(&self, vaddr: VirtAddr) -> Option<axfs::CachedFile> {
        let area = self.areas.find(vaddr)?;
        match area.backend() {
            Backend::File(file) => Some(file.cache_for_reclaim()),
            // A MAP_PRIVATE fault loads original bytes from the inode cache
            // before materialising its anonymous COW leaf.  It therefore has
            // the same lock-external reclaim retry source as a shared file
            // mapping, even though its eventual PTE is not a cache alias.
            Backend::Cow(cow) => cow.cache_for_reclaim(),
            Backend::Linear(_) | Backend::Shared(_) => None,
        }
    }

    /// Retains every file cache intersecting an explicit population request.
    ///
    /// This is called only after `populate_area` returned its internal
    /// `ResourceBusy` token.  The caller releases the address-space lock
    /// before reclaiming these caches, then repeats its complete VMA and
    /// permission validation.  Deduplication keeps a split file VMA from
    /// turning one retry into redundant eviction transactions.
    pub(crate) fn file_caches_for_population_retry(
        &self,
        start: VirtAddr,
        size: usize,
    ) -> AxResult<Vec<axfs::CachedFile>> {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let mut caches: Vec<axfs::CachedFile> = Vec::new();
        for area in self.areas.iter() {
            if area.end() <= start || area.start() >= end {
                continue;
            }
            let cache = match area.backend() {
                Backend::File(file) => file.cache_for_reclaim(),
                Backend::Cow(cow) => match cow.cache_for_reclaim() {
                    Some(cache) => cache,
                    None => continue,
                },
                Backend::Linear(_) | Backend::Shared(_) => continue,
            };
            if caches.iter().any(|existing| existing.ptr_eq(&cache)) {
                continue;
            }
            caches.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            caches.push(cache);
        }
        Ok(caches)
    }

    /// Handles a page fault at the given address.
    ///
    /// `access_flags` indicates the access type that caused the page fault.
    ///
    /// Returns the outcome of the page fault handling.
    pub fn handle_page_fault_result(
        &mut self,
        vaddr: VirtAddr,
        access_flags: PageFaultFlags,
        user_sp: Option<VirtAddr>,
    ) -> PageFaultResult {
        if !self.va_range.contains(vaddr) {
            return PageFaultResult::Failed(PageFaultFailure::AddressNotMapped);
        }
        if self.is_madvise_guard(vaddr.align_down(PAGE_SIZE_4K)) {
            return PageFaultResult::Failed(PageFaultFailure::AccessDenied);
        }
        if self.is_madvise_hwpoison(vaddr.align_down(PAGE_SIZE_4K)) {
            return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
        }
        if let Some((flags, backend, area_end)) = self
            .areas
            .find(vaddr)
            .map(|area| (area.flags(), area.backend().clone(), area.end()))
        {
            // PFEC.SS is write-like only for a SHSTK VMA which retained its
            // shadow-write policy.  A read-only/PROT_NONE SHSTK VMA keeps the
            // type for later mprotect but cannot instantiate or COW a D=1
            // leaf merely because the fault carries PFEC.SS.
            if access_flags.contains(MappingFlags::SHADOW_STACK)
                && !flags.contains(MappingFlags::WRITE)
            {
                return PageFaultResult::Failed(PageFaultFailure::AccessDenied);
            }
            if flags.contains(access_flags) {
                // A CET access is permissioned by SHADOW_STACK, but still
                // has write semantics for private-COW allocation/copy.
                let populate_access = if access_flags.contains(MappingFlags::SHADOW_STACK)
                    && !self.borrowed_cet_shadow_stack_contains(vaddr)
                {
                    access_flags | MappingFlags::WRITE
                } else {
                    access_flags
                };
                let page = vaddr.align_down(PAGE_SIZE_4K);
                if let Some(entry) = self.swapped.get(&page).copied() {
                    let restored = {
                        let mut cursor = self.pt.cursor();
                        backend.restore_swapped_page(page, flags, entry, &mut cursor)
                    };
                    match restored {
                        Ok(()) => {
                            self.swapped.remove(&page);
                            self.publish_resident_highwater();
                            return PageFaultResult::Handled;
                        }
                        Err(error) if error.canonicalize() == AxError::NoMemory => {
                            return PageFaultResult::Failed(PageFaultFailure::OutOfMemory);
                        }
                        Err(_) => {
                            return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
                        }
                    }
                }
                let page_size = backend.page_size();
                let start = vaddr.align_down(page_size);
                if backend.faults_with_sigbus(start) {
                    return PageFaultResult::Failed(PageFaultFailure::BackingUnavailable);
                }
                let page = vaddr.align_down(PAGE_SIZE_4K);
                if access_flags.contains(MappingFlags::WRITE)
                    && self.consume_madvise_free_write_fault(page, flags)
                {
                    // The protected lazy-free leaf has been made writable
                    // again. Retry the exact instruction; no backend/COW
                    // population is needed for this already resident page.
                    return PageFaultResult::Handled;
                }
                let fault_around = backend.fault_around_size(populate_access);
                let fault_around_len = area_end
                    .sub_addr(start)
                    .min(fault_around.max(page_size as usize));
                let leaf_state = match self.pt.query(page) {
                    Ok((_paddr, page_flags, _page_size))
                        if present_leaf_satisfies_fault(page_flags, access_flags) =>
                    {
                        // A remote resolver/fault may have published this
                        // formerly absent leaf after the hardware cached an
                        // invalid translation. Repair only the fault-receiving
                        // CPU; a global shootdown on every fresh map would put
                        // the wrong ownership and cost on the publisher.
                        super::super::repair_local_spurious_fault(vaddr);
                        return PageFaultResult::Handled;
                    }
                    Ok(_) => UffdFaultLeafState::Present,
                    Err(PagingError::NotMapped) => UffdFaultLeafState::Missing,
                    Err(_) => {
                        return PageFaultResult::Failed(PageFaultFailure::InternalInconsistency);
                    }
                };
                let len = self.ordinary_fault_prefix_before_uffd(
                    vaddr,
                    start,
                    page_size as usize,
                    fault_around_len,
                    leaf_state,
                );
                if len == 0 {
                    // User-originated registered faults are intercepted by
                    // `FaultSession`; kernel-originated USER_MODE_ONLY faults
                    // are rejected by `handle_page_fault` below. Reaching the
                    // ordinary population path for the registered page would
                    // violate both boundaries, so fail closed.
                    return PageFaultResult::Failed(PageFaultFailure::InternalInconsistency);
                }
                let populate_outcome = backend.populate(
                    VirtAddrRange::from_start_size(start, len),
                    flags,
                    populate_access,
                    &mut self.pt.cursor(),
                );
                let populate_result = populate_outcome.finish(self);
                // Synchronize even on error: a multi-page populate may have
                // installed a valid executable prefix before the failure.
                synchronize_executable_publication(flags);
                self.publish_resident_highwater();
                match populate_result {
                    Ok(n) => {
                        if n == 0 {
                            debug!("No pages populated for {vaddr:?} ({flags:?})");
                        }
                    }
                    // ResourceBusy is the file cache's internal retry token:
                    // the fault session reclaims one page and retries, so it
                    // is not a population failure worth logging.
                    Err(err) if err.canonicalize() == AxError::ResourceBusy => {}
                    Err(err) => {
                        debug!("Failed to populate pages for {vaddr:?} ({flags:?}): {err}");
                    }
                }
                return classify_page_population(populate_result);
            }
            return PageFaultResult::Failed(PageFaultFailure::AccessDenied);
        }
        self.try_handle_growdown_fault(vaddr, access_flags, user_sp)
    }

    /// Returns `true` if the page fault is handled successfully (not a real
    /// fault).
    pub fn handle_page_fault(&mut self, vaddr: VirtAddr, access_flags: PageFaultFlags) -> bool {
        if self.blocks_kernel_usercopy_missing(vaddr) {
            return false;
        }
        let fault_candidate = self.fault_needs_accounting(vaddr, access_flags);
        let read_before = axtask::current_may_uninit().and_then(|current| {
            current
                .try_as_thread()
                .map(|thread| thread.backing_read_bytes())
        });
        let handled = matches!(
            self.handle_page_fault_result(vaddr, access_flags, None),
            PageFaultResult::Handled
        );
        if handled
            && fault_candidate
            && let (Some(current), Some(read_before)) = (axtask::current_may_uninit(), read_before)
            && let Some(thread) = current.try_as_thread()
        {
            thread.account_resolved_page_fault(read_before);
        }
        handled
    }
}
