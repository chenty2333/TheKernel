//! AddrSpace: brk and mapping creation.

use super::*;

impl AddrSpace {
    /// Returns the stable kernel-only owner identity of the mapping covering
    /// `address`.  This is intentionally narrower than exposing a finalizer
    /// clone: syscall lifetime managers use the identity only to prove that a
    /// user-supplied address still belongs to the admitted logical mapping.
    pub(crate) fn mapping_finalizer_identity_at(&self, address: VirtAddr) -> Option<usize> {
        self.find_area(address)?
            .backend()
            .mapping_finalizer()
            .map(DeferredMappingFinalizer::identity)
    }

    /// Captures every live VMA fragment owned by one logical mapping lease.
    ///
    /// The returned ranges are exact current VMA boundaries in ascending
    /// order.  Reserving for the complete area count up front makes this safe
    /// to use as the read-only preparation half of a later teardown
    /// transaction: allocation cannot fail after a prefix has been captured.
    pub(crate) fn mapping_ranges_with_finalizer(
        &self,
        identity: usize,
    ) -> AxResult<Vec<VirtAddrRange>> {
        let mut ranges = Vec::new();
        ranges
            .try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            if area
                .backend()
                .mapping_finalizer()
                .is_some_and(|finalizer| finalizer.identity() == identity)
            {
                ranges.push(area.va_range());
            }
        }
        Ok(ranges)
    }

    /// Snapshot live finalizer identities without taking any subsystem lock.
    /// Fork uses this before acquiring IPC metadata so retired SysV records
    /// awaiting their deferred finalizer cannot be inherited as live VMAs.
    pub(crate) fn mapping_finalizer_identities(&self) -> AxResult<Vec<usize>> {
        let mut identities = Vec::new();
        identities
            .try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            if let Some(finalizer) = area.backend().mapping_finalizer() {
                let identity = finalizer.identity();
                if !identities.contains(&identity) {
                    identities.push(identity);
                }
            }
        }
        Ok(identities)
    }

    /// Retains one lease for each logical mapping owner intersecting `range`.
    ///
    /// Fixed replacement callers use these preallocated leases to determine,
    /// after publication, which owners lost their final VMA fragment.  The
    /// retained lease prevents the task-context finalizer from racing that
    /// decision; subsystem metadata can then be retired synchronously after
    /// the MM lock is released and the deferred finalizer becomes an exact
    /// no-op.
    pub(crate) fn mapping_finalizers_in_range(
        &self,
        start: VirtAddr,
        size: usize,
    ) -> AxResult<Vec<DeferredMappingFinalizer>> {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let range = VirtAddrRange::new(start, end);
        let mut finalizers = Vec::new();
        finalizers
            .try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            if !area.va_range().overlaps(range) {
                continue;
            }
            let Some(finalizer) = area.backend().mapping_finalizer() else {
                continue;
            };
            if finalizers
                .iter()
                .any(|existing: &DeferredMappingFinalizer| {
                    existing.identity() == finalizer.identity()
                })
            {
                continue;
            }
            finalizers.push(finalizer.clone());
        }
        Ok(finalizers)
    }

    pub(crate) fn has_mapping_finalizer_identity(&self, identity: usize) -> bool {
        self.areas.iter().any(|area| {
            area.backend()
                .mapping_finalizer()
                .is_some_and(|finalizer| finalizer.identity() == identity)
        })
    }

    /// Returns logical-owner candidates whose shared-object page offset has
    /// the same relation to `address` as Linux's SysV `shmdt` VMA scan.
    ///
    /// This intentionally starts at the first VMA at or above `address`.
    /// Partial munmap/mprotect may remove or split the original first VMA,
    /// while an mremap may establish a new address whose offset geometry is
    /// independently valid. Namespace provenance is resolved by the caller
    /// from the returned finalizer identities.
    pub(crate) fn sysv_shmdt_finalizer_candidates(
        &self,
        address: VirtAddr,
    ) -> AxResult<Vec<usize>> {
        let mut candidates = Vec::new();
        candidates
            .try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            if area.end() <= address || area.start() < address {
                continue;
            }
            let Some(finalizer) = area.backend().mapping_finalizer() else {
                continue;
            };
            let Some(object_offset) = self.shared_backing_offset_at(area.start()) else {
                continue;
            };
            let displacement = area.start().sub_addr(address);
            if displacement / PAGE_SIZE_4K != object_offset / PAGE_SIZE_4K {
                continue;
            }
            let identity = finalizer.identity();
            if !candidates.contains(&identity) {
                candidates.push(identity);
            }
        }
        Ok(candidates)
    }

    /// Captures the exact VMA fragments selected by Linux's SysV `shmdt`
    /// offset rule after a candidate finalizer has been resolved to a segment.
    pub(crate) fn sysv_shmdt_mapping_ranges(
        &self,
        identity: usize,
        address: VirtAddr,
        segment_size: usize,
    ) -> AxResult<Vec<VirtAddrRange>> {
        let end = address
            .checked_add(segment_size)
            .ok_or(AxError::InvalidInput)?;
        let mut ranges = Vec::new();
        ranges
            .try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut found_initial = false;
        for area in self.areas.iter() {
            if area.end() <= address || area.start() < address {
                continue;
            }
            if found_initial && area.end() > end {
                break;
            }
            let matches_owner = area
                .backend()
                .mapping_finalizer()
                .is_some_and(|finalizer| finalizer.identity() == identity);
            let matches_offset =
                self.shared_backing_offset_at(area.start())
                    .is_some_and(|object_offset| {
                        area.start().sub_addr(address) / PAGE_SIZE_4K
                            == object_offset / PAGE_SIZE_4K
                    });
            if matches_owner && matches_offset {
                ranges.push(area.va_range());
                found_initial = true;
            }
        }
        Ok(ranges)
    }

    /// Atomically replaces every inherited reference to one mapping finalizer.
    ///
    /// Fork copies VMA backends before the child is published. SysV SHM must
    /// then give the child its own logical attachment rather than sharing the
    /// parent's finalizer. The sparse prepared metadata transaction builds a
    /// complete replacement tree first and commits it with one swap, so an
    /// allocation failure cannot leave parent and child ownership mixed in the
    /// same address space.
    pub(crate) fn rebind_mapping_finalizer(
        &mut self,
        old_identity: usize,
        replacement: DeferredMappingFinalizer,
    ) -> AxResult<usize> {
        let matching = self
            .areas
            .iter()
            .filter(|area| {
                area.backend()
                    .mapping_finalizer()
                    .is_some_and(|finalizer| finalizer.identity() == old_identity)
            })
            .count();
        if matching == 0 {
            return Err(AxError::NotFound);
        }

        let next_topology_generation = self.next_topology_generation()?;
        let prepared = self
            .areas
            .prepare_matching_metadata_update_with_limit(
                |backend| {
                    backend
                        .mapping_finalizer()
                        .is_some_and(|finalizer| finalizer.identity() == old_identity)
                },
                |backend| backend.replace_mapping_finalizer(Some(replacement.clone())),
                MAX_VMA_FRAGMENTS,
            )
            .map_err(AxError::from)?;
        if !prepared.changed() {
            return Err(AxError::BadState);
        }
        prepared
            .commit(&mut self.areas)
            .map_err(AxError::from)?
            .finish();
        self.commit_topology_generation(next_topology_generation);
        Ok(matching)
    }

    /// Removes every VMA fragment owned by one logical mapping finalizer.
    pub(crate) fn unmap_mapping_finalizer_fragments(
        &mut self,
        identity: usize,
    ) -> AxResult<DeferredUffdWake> {
        let ranges = self.mapping_ranges_with_finalizer(identity)?;
        self.unmap_mapping_finalizer_ranges(identity, &ranges)
    }

    /// Removes an exact subset of one logical mapping owner's VMA fragments.
    ///
    /// All selected VMA/backend retirements and UFFD/mapping-identity sidecars
    /// are prepared before the first removal; retired finalizer references are
    /// released only after one translation grace period covers every range.
    /// If unselected fragments retain the same owner, its finalizer remains
    /// live and the higher-level attachment is not detached.
    pub(crate) fn unmap_mapping_finalizer_ranges(
        &mut self,
        identity: usize,
        ranges: &[VirtAddrRange],
    ) -> AxResult<DeferredUffdWake> {
        if ranges.is_empty() {
            return Err(AxError::NotFound);
        }
        let mut previous_end = None;
        for range in ranges {
            if range.start >= range.end
                || previous_end.is_some_and(|previous_end| previous_end > range.start)
            {
                return Err(AxError::InvalidInput);
            }
            previous_end = Some(range.end);
            let size = range.end.sub_addr(range.start);
            self.validate_region(range.start, size)?;
            self.check_no_seal_overlap(range.start, size)?;
            self.check_no_user_io_pin_overlap(range.start, size, InvalidationReason::Unmap)?;
        }

        self.dontdump_ranges
            .try_reserve(ranges.len())
            .map_err(|_| AxError::NoMemory)?;
        for range in ranges {
            let size = range.end.sub_addr(range.start);
            crate::uprobe::invalidate_xol_range_locked(self, range.start, size);
            self.ensure_4k_granularity(range.start, size)?;
        }

        // 4 KiB restoration may split a selected VMA. Rebuild the complete
        // exact-area selection after that step while requiring continuous
        // coverage and the same finalizer identity throughout every requested
        // range. This is still preparation: no VMA or PTE has been removed.
        let mut selected_starts = Vec::new();
        selected_starts
            .try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for range in ranges {
            let mut cursor = range.start;
            for area in self.areas.iter_overlapping(*range) {
                if area.start() != cursor || area.end() > range.end {
                    return Err(AxError::BadState);
                }
                if area
                    .backend()
                    .mapping_finalizer()
                    .is_none_or(|finalizer| finalizer.identity() != identity)
                {
                    return Err(AxError::BadState);
                }
                selected_starts.push(area.start());
                cursor = area.end();
            }
            if cursor != range.end {
                return Err(AxError::BadState);
            }
        }

        let next_generation = self.next_topology_generation()?;
        let mapping_mutations = prepare_unmap_mapping_mutations_for_ranges(
            &self.areas,
            &self.mapping_identities,
            ranges,
        )?;

        let mut page_ranges = Vec::new();
        page_ranges
            .try_reserve(ranges.len())
            .map_err(|_| AxError::NoMemory)?;
        for range in ranges {
            page_ranges.push(
                PageRange::new(
                    range.start.as_usize(),
                    range.end.sub_addr(range.start),
                    PAGE_SIZE_4K,
                )
                .map_err(mm_error)?,
            );
        }
        let uffd_plan = {
            let AddrSpace {
                address_space_id,
                areas,
                mapping_identities,
                uffd,
                ..
            } = self;
            let address_space_id = *address_space_id;
            if let Some(state) = uffd.as_deref_mut() {
                match state.preflight_unmap_range_slice(0, &page_ranges, |registration| {
                    Self::uffd_snapshot_for_registration(
                        address_space_id,
                        areas,
                        mapping_identities,
                        registration,
                    )
                })? {
                    OptionalUffdPlan::Noop => None,
                    plan @ OptionalUffdPlan::Armed(_) => Some(plan),
                }
            } else {
                None
            }
        };

        self.publish_resident_highwater();
        let retirement = match self.areas.unmap_selected_deferred_with_limit(
            &selected_starts,
            |backend| {
                backend
                    .mapping_finalizer()
                    .is_some_and(|finalizer| finalizer.identity() == identity)
            },
            &mut self.pt,
            MAX_VMA_FRAGMENTS,
        ) {
            Ok(retirement) => retirement,
            Err(error) => {
                if let Some(plan) = uffd_plan {
                    self.uffd
                        .as_mut()
                        .expect("armed UFFD fragment-unmap plan lost its state")
                        .abort_plan(plan);
                }
                return Err(error.into());
            }
        };
        for range in ranges {
            self.release_swapped_range(range.start, range.end.sub_addr(range.start));
        }
        let grace = self.synchronize_tlb_after_mutation();
        retirement.release();
        drop(grace);
        self.publish_resident_highwater();

        self.refresh_growdown_starts();
        for range in ranges {
            let start = range.start;
            let end = range.end;
            let size = end.sub_addr(start);
            self.madvise_free_pages
                .retain(|&page, _| page < start || page >= end);
            Self::clear_interval(&mut self.madvise_guard_ranges, start, size);
            Self::clear_interval(&mut self.madvise_hwpoison_ranges, start, size);
            Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
            Self::clear_interval(&mut self.dontfork_ranges, start, size);
            Self::clear_interval_vec(&mut self.dontdump_ranges, start, size);
            self.clear_locked_range(start, size);
        }
        self.prune_shared_alias_bindings();
        let wake = if let Some(plan) = uffd_plan {
            self.uffd
                .as_mut()
                .expect("armed UFFD fragment-unmap plan lost its state")
                .commit_plan(plan)
        } else {
            DeferredUffdWake::empty()
        };
        commit_mapping_identity_mutations(&mut self.mapping_identities, &mapping_mutations);
        self.commit_topology_generation(next_generation);
        Ok(wake)
    }

    /// Returns the strong backing identity and byte offset for a mapped
    /// process-shared futex word.  The caller must hold this address space's
    /// mutex while using the result for a queue operation; a later no-fault
    /// check compares the live mapping against this exact lease to reject
    /// remap/unmap ABA races.
    pub(crate) fn futex_shared_key_at(&self, address: usize) -> Option<SharedFutexKey> {
        let end = address.checked_add(size_of::<u32>())?;
        let area = self.find_area(VirtAddr::from_usize(address))?;
        if address < area.start().as_usize() || end > area.end().as_usize() {
            return None;
        }
        area.backend().futex_shared_key(address)
    }

    /// Gate-safe shared-futex identity lookup.  Unlike key derivation this
    /// returns only copyable identity data and never clones the backing lease.
    pub(crate) fn futex_shared_id_at(
        &self,
        address: usize,
    ) -> Option<(crate::mm::FutexBackingId, crate::mm::FutexWordOffset)> {
        let end = address.checked_add(size_of::<u32>())?;
        let area = self.find_area(VirtAddr::from_usize(address))?;
        if address < area.start().as_usize() || end > area.end().as_usize() {
            return None;
        }
        area.backend().futex_shared_id(address)
    }

    pub fn brk_growth_collides(&self, start: VirtAddr, end: VirtAddr, heap_base: VirtAddr) -> bool {
        if start >= end {
            return false;
        }

        for area in self.areas.iter() {
            if area.end() <= start {
                continue;
            }
            if area.start() >= end {
                break;
            }

            if !is_brk_heap_area(area, heap_base) {
                return true;
            }
        }

        false
    }

    /// Linux `SYSCALL_DEFINE1(brk)`'s stack-guard-gap rejection.
    ///
    /// Linux v7.2.3 `mm/mmap.c`:
    ///
    /// ```c
    /// 	/*
    /// 	 * Only check if the next VMA is within the stack_guard_gap of the
    /// 	 * expansion area
    /// 	 */
    /// 	vma_iter_init(&vmi, mm, oldbrk);
    /// 	next = vma_find(&vmi, newbrk + PAGE_SIZE + stack_guard_gap);
    /// 	if (next && newbrk + PAGE_SIZE > vm_start_gap(next))
    /// 		goto out;
    /// ```
    ///
    /// `vma_iter_init(&vmi, mm, oldbrk)` starts the search at the old break, so
    /// a VMA that merely *ends* at the old break — the brk VMA itself, and the
    /// area `brk_growth_collides` exempts — is never `next`; only a VMA that
    /// extends past `oldbrk` can be.  `expand_start` is this kernel's effective
    /// growth start (`max(initial_heap_end, oldbrk)`) and the heap area below it
    /// is skipped for the same reason.  Everything else mirrors the search
    /// bound `newbrk + PAGE_SIZE + stack_guard_gap` and the strict `>` against
    /// `vm_start_gap()`.
    ///
    /// Unlike `brk_growth_collides`, this predicate is not about the mapping a
    /// growth would overwrite: a `VM_GROWSDOWN` VMA (or a shadow stack) above
    /// the new break owns reserved address space *below* its start, and Linux
    /// refuses a break that would grow into it.
    pub fn brk_growth_crosses_next_guard_gap(
        &self,
        expand_start: VirtAddr,
        new_brk: VirtAddr,
        heap_base: VirtAddr,
    ) -> bool {
        let Some(window_end) = new_brk
            .as_usize()
            .checked_add(PAGE_SIZE_4K + tk_linux_mm::STACK_GUARD_GAP_DEFAULT as usize)
        else {
            return true;
        };
        let Some(next) = self.areas.iter().find(|area| {
            area.start() >= expand_start
                && area.start().as_usize() < window_end
                && !is_brk_heap_area(area, heap_base)
        }) else {
            return false;
        };
        let gap = if self.growdown_starts.contains(&next.start()) {
            tk_linux_mm::StartGap::GrowDown
        } else if next.flags().contains(MappingFlags::SHADOW_STACK) {
            tk_linux_mm::StartGap::ShadowStack
        } else {
            tk_linux_mm::StartGap::None
        };
        tk_linux_mm::growth_crosses_next_guard_gap(
            new_brk.as_usize() as u64,
            next.start().as_usize() as u64,
            gap,
            tk_linux_mm::STACK_GUARD_GAP_DEFAULT,
        )
    }

    /// Add a new linear mapping.
    ///
    /// See [`Backend`] for more details about the mapping backends.
    ///
    /// The `flags` parameter indicates the mapping permissions and attributes.
    ///
    /// Returns an error if the address range is out of the address space or not
    /// aligned.
    pub fn map_linear(
        &mut self,
        start_vaddr: VirtAddr,
        start_paddr: PhysAddr,
        size: usize,
        flags: MappingFlags,
    ) -> AxResult {
        self.validate_region(start_vaddr, size)?;
        let next_generation = self.next_topology_generation()?;
        let lineage = self.prepare_fresh_mapping_lineage()?;

        if !start_paddr.is_aligned_4k() {
            let removed = self.remove_mapping_lineage_if_unused(lineage);
            debug_assert!(removed);
            ax_bail!(InvalidInput, "address is not aligned");
        }

        let area = MemoryArea::new_with_lineage(
            start_vaddr,
            size,
            flags,
            Backend::new_linear(start_vaddr, start_paddr, size),
            lineage,
        );
        if let Err(error) = self
            .areas
            .map_with_limit(area, &mut self.pt, false, MAX_VMA_FRAGMENTS)
        {
            let removed = self.remove_mapping_lineage_if_unused(lineage);
            debug_assert!(removed);
            return Err(error.into());
        }
        self.commit_topology_generation(next_generation);
        Ok(())
    }

    pub fn map(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
    ) -> AxResult {
        self.map_with_lock_state(
            start,
            size,
            flags,
            populate,
            backend,
            self.lock_future_mappings,
        )
    }

    /// Replace one complete shared VMA at the same virtual address with an
    /// alias of its backing at `page_offset`.  The caller holds the address
    /// space mutex for the entire prepared replacement; both the replacement
    /// and rollback backends are fully constructed before the old PTE/VMA is
    /// retired.  Deferred UFFD wakeup is deliberately returned to the caller
    /// and must be finished only after dropping that mutex.
    pub(crate) fn replace_shared_mapping_at_offset(
        &mut self,
        aspace: &Arc<Mutex<AddrSpace>>,
        start: VirtAddr,
        size: usize,
        page_offset: usize,
        populate: bool,
    ) -> AxResult<DeferredUffdWake> {
        self.validate_region(start, size)?;
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let area = self.find_area(start).ok_or(AxError::InvalidInput)?;
        if area.start() != start || area.end() < end {
            return Err(AxError::InvalidInput);
        }
        let flags = area.flags();
        let locked = self.range_is_locked(start, size);
        let source = area.backend();
        let shared = source
            .file_mapping()
            .is_some_and(|mapping| mapping.sharing() == FileMappingSharing::Shared)
            || matches!(source, Backend::Shared(_));
        if !shared {
            return Err(AxError::InvalidInput);
        }
        // Build both candidates before retiring the old VMA.  Recreating the
        // original through relocate(start,start) retains every cache/lease
        // registration should publication of the replacement fail.
        let rollback = source.relocate(start, start, aspace)?;
        let replacement = match source {
            Backend::File(_) => source.clone_file_rebased(start, page_offset, aspace)?,
            Backend::Shared(_) => source.clone_shared_rebased(start, page_offset)?,
            Backend::Linear(_) | Backend::Cow(_) => return Err(AxError::InvalidInput),
        };
        let wake = self.unmap(start, size)?;
        match self.map_with_lock_state(start, size, flags, populate, replacement, locked) {
            Ok(()) => Ok(wake),
            Err(error) => {
                // The old mapping was retained as a prepared backend, so a
                // failed fixed replacement cannot leave a hole visible after
                // the address-space lock is released.
                self.map_with_lock_state(start, size, flags, false, rollback, locked)
                    .map_err(|_| AxError::BadState)?;
                Err(error)
            }
        }
    }

    /// Snapshot the complete Linux remap_file_pages VMA span.  Every fragment
    /// must be contiguous, shared, carry the same flags and pin the same OFD.
    pub(crate) fn remap_shared_span_snapshot(
        &self,
        start: VirtAddr,
        size: usize,
    ) -> AxResult<(MappingFlags, FileMappingLease)> {
        let range = VirtAddrRange::try_from_start_size(start, size).ok_or(AxError::InvalidInput)?;
        let mut cursor = start;
        let mut result: Option<(MappingFlags, FileMappingLease)> = None;
        for area in self.areas_overlapping(range) {
            if cursor >= range.end {
                break;
            }
            if area.start() > cursor {
                return Err(AxError::InvalidInput);
            }
            let lease = area.backend().file_mapping().ok_or(AxError::InvalidInput)?;
            if lease.sharing() != FileMappingSharing::Shared {
                return Err(AxError::InvalidInput);
            }
            if let Some((flags, first)) = &result {
                if *flags != area.flags() || first.ofd_key() != lease.ofd_key() {
                    return Err(AxError::InvalidInput);
                }
            } else {
                result = Some((area.flags(), lease.clone()));
            }
            cursor = area.end().min(range.end);
        }
        if cursor != range.end {
            return Err(AxError::InvalidInput);
        }
        result.ok_or(AxError::InvalidInput)
    }

    pub(crate) fn replace_shared_mapping_span_at_offset(
        &mut self,
        aspace: &Arc<Mutex<AddrSpace>>,
        start: VirtAddr,
        size: usize,
        page_offset: usize,
        _populate: bool,
    ) -> LockExternalUffdOutcome<(), AxError> {
        let mut deferred_wake = DeferredUffdWake::empty();
        let outcome = (|| -> AxResult {
            let range =
                VirtAddrRange::try_from_start_size(start, size).ok_or(AxError::InvalidInput)?;
            self.check_no_seal_overlap(start, size)?;
            self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Unmap)?;
            let source_mutations = prepare_unmap_mapping_mutations(
                &self.areas,
                &self.mapping_identities,
                start,
                size,
            )?;
            let next_topology_generation = self.next_topology_generation()?;
            let policy = self.prepare_remap_policy(start, size, size)?;
            // `mlock` can cover only a prefix, suffix, or interior pages of a
            // VMA.  `prepare_remap_policy` snapshots those exact intervals before
            // unmap clears the ledger, so the replacement neither drops the
            // charge nor expands it to the whole VMA.
            let mut cursor = start;
            let mut fragments: Vec<(
                VirtAddr,
                usize,
                MappingFlags,
                MappingLineage,
                Backend,
                Backend,
            )> = Vec::new();
            for area in self.areas_overlapping(range) {
                if cursor >= range.end {
                    break;
                }
                if area.start() > cursor {
                    return Err(AxError::InvalidInput);
                }
                let end = area.end().min(range.end);
                let length = end.sub_addr(cursor);
                let offset = page_offset
                    .checked_add(cursor.sub_addr(start) / PAGE_SIZE_4K)
                    .ok_or(AxError::InvalidInput)?;
                let source = area.backend();
                let shared = source
                    .file_mapping()
                    .is_some_and(|lease| lease.sharing() == FileMappingSharing::Shared);
                if !shared {
                    return Err(AxError::InvalidInput);
                }
                let rollback = source.relocate(cursor, cursor, aspace)?;
                let replacement = match source {
                    Backend::File(_) => source.clone_file_rebased(cursor, offset, aspace)?,
                    Backend::Shared(_) => source.clone_shared_rebased(cursor, offset)?,
                    _ => return Err(AxError::InvalidInput),
                };
                fragments.push((
                    cursor,
                    length,
                    area.flags(),
                    area.lineage(),
                    rollback,
                    replacement,
                ));
                cursor = end;
            }
            if cursor != range.end {
                return Err(AxError::InvalidInput);
            }
            // Arm UFFD only after every backend and rollback candidate has been
            // built.  From here, every failure consumes this plan exactly once.
            // Do not use `unmap()` below: that helper commits its UFFD plan
            // immediately.  A nonlinear replacement must retain both outcomes
            // until the replacement has either fully published or been restored.
            let uffd_plan =
                self.preflight_remap_uffd(UffdRemapKind::Move, true, start, size, start, size)?;
            if let Err(error) = self.unmap_areas_with_tlb_grace(start, size) {
                deferred_wake
                    .merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Preserved));
                return Err(error.into());
            }
            self.refresh_growdown_starts();
            Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
            Self::clear_interval(&mut self.dontfork_ranges, start, size);
            Self::clear_interval_vec(&mut self.dontdump_ranges, start, size);
            self.clear_locked_range(start, size);
            for (committed, (_, _, flags, _, _, replacement)) in fragments.iter().enumerate() {
                let (address, length, ..) = fragments[committed];
                if let Err(error) = self.map_with_lock_state(
                    address,
                    length,
                    *flags,
                    false,
                    replacement.clone(),
                    false,
                ) {
                    let replacement_mutations = prepare_unmap_mapping_mutations(
                        &self.areas,
                        &self.mapping_identities,
                        start,
                        size,
                    )
                    .ok();
                    let restored = self.unmap_areas_with_tlb_grace(start, size).is_ok();
                    if restored && let Some(replacement_mutations) = replacement_mutations {
                        commit_mapping_identity_mutations(
                            &mut self.mapping_identities,
                            &replacement_mutations,
                        );
                    }
                    for (address, length, flags, lineage, rollback, _) in fragments.into_iter() {
                        if self
                            .map_with_existing_lineage(
                                address, length, flags, false, rollback, false, lineage,
                            )
                            .is_err()
                        {
                            deferred_wake.merge(self.resolve_remap_uffd(
                                uffd_plan,
                                RemapUffdOutcome::DestructiveFailure,
                            ));
                            self.commit_topology_generation(next_topology_generation);
                            return Err(AxError::BadState);
                        }
                    }
                    if restored {
                        self.apply_remap_policy(start, &policy);
                        deferred_wake
                            .merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Preserved));
                        return Err(error);
                    }
                    deferred_wake.merge(
                        self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::DestructiveFailure),
                    );
                    self.commit_topology_generation(next_topology_generation);
                    return Err(AxError::BadState);
                }
            }
            self.apply_remap_policy(start, &policy);
            commit_mapping_identity_mutations(&mut self.mapping_identities, &source_mutations);
            self.commit_topology_generation(next_topology_generation);
            deferred_wake.merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Committed));
            Ok(())
        })();
        LockExternalUffdOutcome::new(outcome, deferred_wake)
    }

    pub fn map_with_lock_state(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
        locked: bool,
    ) -> AxResult {
        self.validate_region(start, size)?;
        let next_generation = self.next_topology_generation()?;
        let lineage = self.prepare_fresh_mapping_lineage()?;

        let area = MemoryArea::new_with_lineage(start, size, flags, backend, lineage);
        if let Err(error) = self
            .areas
            .map_with_limit(area, &mut self.pt, false, MAX_VMA_FRAGMENTS)
        {
            let removed = self.remove_mapping_lineage_if_unused(lineage);
            debug_assert!(removed);
            return Err(error.into());
        }
        // Population or its rollback may partially change resident state, so
        // publish the new topology generation as soon as the VMA is visible.
        self.commit_topology_generation(next_generation);
        if locked {
            self.insert_locked_range(start, start + size);
        }
        if populate && let Err(err) = self.populate_area(start, size, flags) {
            if let Err(unmap_err) = self.unmap_areas_with_tlb_grace(start, size) {
                warn!(
                    "AddrSpace::map: failed to roll back {start:?}+{size:#x} after populate \
                     error: {unmap_err:?}"
                );
            }
            self.refresh_growdown_starts();
            self.clear_locked_range(start, size);
            // A fail-stop backend can leave the area visible after rollback
            // failure. Keep its sidecar identity in that case; removing it
            // would make later snapshots silently lose their lineage state.
            self.remove_mapping_lineage_if_unused(lineage);
            return Err(err);
        }
        Ok(())
    }

    pub(super) fn rollback_fixed_replacement_participant<P: FixedReplacementParticipant>(
        &mut self,
        participant: &mut P,
        error: AxError,
    ) -> ReplaceMappingError {
        ReplaceMappingError::AddressSpacePreserved(
            participant.rollback(self).err().unwrap_or(error),
        )
    }
}
