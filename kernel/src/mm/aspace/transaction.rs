//! AddrSpace: fixed replacement and mremap/duplicate mapping transactions.

use super::*;

impl AddrSpace {
    /// Installs one MAP_FIXED-style replacement as a single VMA/PTE
    /// transaction.
    ///
    /// The sibling memory-set guard keeps the exact old VMA tree and prepared
    /// PTE retirement tokens alive until the incoming mapping has installed.
    /// Old finalizers/backends are released only after this address space's
    /// global translation grace.  Every policy sidecar remains untouched until
    /// that point, so an admitted rollback restores the original mapping and
    /// its logical ownership without a visible hole.
    ///
    /// For a shared incoming backend this deliberately retains old alias
    /// bindings.  The caller must commit its prepared incoming alias lease and
    /// then finish the transition while still holding the address-space lock.
    pub(crate) fn replace_mapping_fixed_with<P: FixedReplacementParticipant>(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        backend: Backend,
        locked: bool,
        participant: &mut P,
    ) -> Result<DeferredUffdWake, ReplaceMappingError> {
        if let Err(error) = self.validate_region(start, size) {
            return Err(self.rollback_fixed_replacement_participant(participant, error));
        }
        if size == 0 {
            return Err(
                self.rollback_fixed_replacement_participant(participant, AxError::InvalidInput)
            );
        }
        if let Err(error) = self.check_no_seal_overlap(start, size) {
            return Err(self.rollback_fixed_replacement_participant(participant, error));
        }
        if let Err(error) =
            self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Unmap)
        {
            return Err(self.rollback_fixed_replacement_participant(participant, error));
        }
        if self.dontdump_ranges.try_reserve(1).is_err() {
            return Err(self.rollback_fixed_replacement_participant(participant, AxError::NoMemory));
        }

        // This may make a huge leaf 4 KiB-granular, but does not retire the
        // VMA, its identity, or any sidecar.  It is the required admission
        // step before the prepared PTE journal observes individual leaves.
        if let Err(error) = self.ensure_4k_granularity(start, size) {
            return Err(self.rollback_fixed_replacement_participant(participant, error));
        }
        let sidecars = match self.prepare_fixed_replacement_sidecars(start, size, locked) {
            Ok(sidecars) => sidecars,
            Err(error) => {
                return Err(self.rollback_fixed_replacement_participant(participant, error));
            }
        };
        let next_topology_generation = match self.next_topology_generation() {
            Ok(generation) => generation,
            Err(error) => {
                return Err(self.rollback_fixed_replacement_participant(participant, error));
            }
        };
        let lineage = match self.prepare_fresh_mapping_lineage() {
            Ok(lineage) => lineage,
            Err(error) => {
                return Err(self.rollback_fixed_replacement_participant(participant, error));
            }
        };
        let replacement = MemoryArea::new_with_lineage(start, size, flags, backend, lineage);
        let prepared = match self.areas.prepare_fixed_replacement_with_limit(
            replacement,
            &self.pt,
            MAX_VMA_FRAGMENTS,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                let removed = self.remove_mapping_lineage_if_unused(lineage);
                debug_assert!(removed);
                return Err(self.rollback_fixed_replacement_participant(participant, error.into()));
            }
        };
        let mapping_mutations = match prepare_unmap_mapping_mutations(
            &self.areas,
            &self.mapping_identities,
            start,
            size,
        ) {
            Ok(mutations) => mutations,
            Err(error) => {
                let removed = self.remove_mapping_lineage_if_unused(lineage);
                debug_assert!(removed);
                return Err(self.rollback_fixed_replacement_participant(participant, error));
            }
        };
        let unmap_range = match PageRange::new(start.as_usize(), size, PAGE_SIZE_4K) {
            Ok(range) => range,
            Err(error) => {
                let removed = self.remove_mapping_lineage_if_unused(lineage);
                debug_assert!(removed);
                return Err(
                    self.rollback_fixed_replacement_participant(participant, mm_error(error))
                );
            }
        };
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
                match state.preflight_unmap(0, unmap_range, |registration| {
                    Self::uffd_snapshot_for_registration(
                        address_space_id,
                        areas,
                        mapping_identities,
                        registration,
                    )
                }) {
                    Ok(OptionalUffdPlan::Noop) => None,
                    Ok(plan @ OptionalUffdPlan::Armed(_)) => Some(plan),
                    Err(error) => {
                        let removed = self.remove_mapping_lineage_if_unused(lineage);
                        debug_assert!(removed);
                        return Err(self.rollback_fixed_replacement_participant(participant, error));
                    }
                }
            } else {
                None
            }
        };

        // Uprobe/XOL state is an externally visible participant in this
        // transaction.  In particular, clearing an XOL identity here would
        // make an otherwise failed replacement lose the old trampoline
        // identity.  Callers performing a user mapping replacement must pass
        // PreparedFixedUprobeTransition, which owns both invalidation and its
        // rollback under the uprobe topology gate.
        if let Err(error) = participant.before_install(self) {
            let participant_rollback = participant.rollback(self).err();
            if let Some(plan) = uffd_plan {
                self.uffd
                    .as_mut()
                    .expect("armed UFFD fixed-replace plan lost its state")
                    .abort_plan(plan);
            }
            let removed = self.remove_mapping_lineage_if_unused(lineage);
            debug_assert!(removed);
            return Err(ReplaceMappingError::AddressSpacePreserved(
                participant_rollback.unwrap_or(error),
            ));
        }
        self.publish_resident_highwater();
        let mut committed = match self
            .areas
            .commit_prepared_fixed_replacement(prepared, &mut self.pt)
        {
            Ok(committed) => committed,
            Err(error) => {
                let participant_rollback = participant.rollback(self).err();
                if let Some(plan) = uffd_plan {
                    self.uffd
                        .as_mut()
                        .expect("armed UFFD fixed-replace plan lost its state")
                        .abort_plan(plan);
                }
                let removed = self.remove_mapping_lineage_if_unused(lineage);
                debug_assert!(removed);
                return Err(ReplaceMappingError::AddressSpacePreserved(
                    participant_rollback.unwrap_or_else(|| error.into()),
                ));
            }
        };
        // The sibling guard has admitted incoming mapping resources and all
        // exact old-leaf restore capacity. Any failure here is therefore a
        // fail-stop backend contract violation, not a recoverable partial map.
        committed.install(&mut self.areas, &mut self.pt);
        if let Err(error) = participant.commit(self) {
            let participant_rollback = participant.rollback(self).err();
            let rollback_retirement = committed.rollback(&mut self.areas, &mut self.pt);
            // Restoring the old leaves invalidates translations cached for
            // the provisional incoming mapping.  Keep its exact VMA/backend
            // ownership alive until every CPU has crossed that invalidation
            // fence; otherwise a stale translation can outlive the incoming
            // backend which supplied it.
            let grace = self.synchronize_tlb_after_mutation();
            rollback_retirement.release();
            drop(grace);
            if let Some(plan) = uffd_plan {
                self.uffd
                    .as_mut()
                    .expect("armed UFFD fixed-replace plan lost its state")
                    .abort_plan(plan);
            }
            let removed = self.remove_mapping_lineage_if_unused(lineage);
            debug_assert!(removed);
            return Err(ReplaceMappingError::AddressSpacePreserved(
                participant_rollback.unwrap_or(error),
            ));
        }
        let retirement = committed.finish();

        // The outgoing software swap entries, VMA owners, and backend
        // finalizers remain live through this single global grace period.
        self.release_swapped_range(start, size);
        let grace = self.synchronize_tlb_after_mutation();
        retirement.release();
        drop(grace);
        self.publish_resident_highwater();

        self.commit_prepared_fixed_replacement_sidecars(sidecars);
        #[cfg(target_arch = "x86_64")]
        self.remove_cet_default_shadow_stack_extents_for_unmap(start, size);
        commit_mapping_identity_mutations(&mut self.mapping_identities, &mapping_mutations);
        self.commit_topology_generation(next_topology_generation);
        let wake = if let Some(plan) = uffd_plan {
            self.uffd
                .as_mut()
                .expect("armed UFFD fixed-replace plan lost its state")
                .commit_plan(plan)
        } else {
            DeferredUffdWake::empty()
        };
        Ok(wake)
    }

    pub(super) fn prepare_fixed_replacement_sidecars(
        &self,
        start: VirtAddr,
        size: usize,
        locked: bool,
    ) -> AxResult<PreparedFixedReplacementSidecars> {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let mut growdown_starts = self.growdown_starts.clone();
        let mut madvise_guard_ranges = self.madvise_guard_ranges.clone();
        let mut madvise_hwpoison_ranges = self.madvise_hwpoison_ranges.clone();
        let mut madvise_free_pages = self.madvise_free_pages.clone();
        let mut wipe_on_fork_ranges = self.wipe_on_fork_ranges.clone();
        let mut dontfork_ranges = self.dontfork_ranges.clone();
        let mut dontdump_ranges = self.dontdump_ranges.clone();
        let mut locked_ranges = self.locked_ranges.clone();

        growdown_starts.retain(|address| *address < start || *address >= end);
        madvise_free_pages.retain(|&page, _| page < start || page >= end);
        Self::clear_interval(&mut madvise_guard_ranges, start, size);
        Self::clear_interval(&mut madvise_hwpoison_ranges, start, size);
        Self::clear_interval(&mut wipe_on_fork_ranges, start, size);
        Self::clear_interval(&mut dontfork_ranges, start, size);
        Self::clear_interval_vec(&mut dontdump_ranges, start, size);
        Self::clear_locked_interval(&mut locked_ranges, start, size);
        if locked {
            Self::insert_locked_interval(&mut locked_ranges, start, end);
        }
        Ok(PreparedFixedReplacementSidecars {
            growdown_starts,
            madvise_guard_ranges,
            madvise_hwpoison_ranges,
            madvise_free_pages,
            wipe_on_fork_ranges,
            dontfork_ranges,
            dontdump_ranges,
            locked_ranges,
        })
    }

    pub(super) fn commit_prepared_fixed_replacement_sidecars(
        &mut self,
        sidecars: PreparedFixedReplacementSidecars,
    ) {
        self.growdown_starts = sidecars.growdown_starts;
        self.madvise_guard_ranges = sidecars.madvise_guard_ranges;
        self.madvise_hwpoison_ranges = sidecars.madvise_hwpoison_ranges;
        self.madvise_free_pages = sidecars.madvise_free_pages;
        self.wipe_on_fork_ranges = sidecars.wipe_on_fork_ranges;
        self.dontfork_ranges = sidecars.dontfork_ranges;
        self.dontdump_ranges = sidecars.dontdump_ranges;
        self.locked_ranges = sidecars.locked_ranges;
    }

    pub(super) fn clear_locked_interval(
        ranges: &mut BTreeMap<VirtAddr, VirtAddr>,
        start: VirtAddr,
        size: usize,
    ) {
        if size == 0 {
            return;
        }
        let end = start + size;
        let overlaps: Vec<_> = ranges
            .range(..end)
            .filter_map(|(&range_start, &range_end)| {
                (range_end > start).then_some((range_start, range_end))
            })
            .collect();
        for (range_start, range_end) in overlaps {
            ranges.remove(&range_start);
            if range_start < start {
                ranges.insert(range_start, start);
            }
            if range_end > end {
                ranges.insert(end, range_end);
            }
        }
    }

    pub(super) fn insert_locked_interval(
        ranges: &mut BTreeMap<VirtAddr, VirtAddr>,
        start: VirtAddr,
        end: VirtAddr,
    ) {
        let mut new_start = start;
        let mut new_end = end;
        let overlaps: Vec<_> = ranges
            .range(..=end)
            .filter_map(|(&range_start, &range_end)| {
                (range_end >= start && range_start <= end).then_some((range_start, range_end))
            })
            .collect();
        for (range_start, range_end) in overlaps {
            ranges.remove(&range_start);
            new_start = new_start.min(range_start);
            new_end = new_end.max(range_end);
        }
        ranges.insert(new_start, new_end);
    }

    pub(super) fn preflight_existing_lineage_tail_uffd(
        &mut self,
        lineage: MappingLineage,
        old_end: VirtAddr,
        new_end: VirtAddr,
    ) -> Result<Option<OptionalUffdPlan>, ExistingLineageMapError> {
        let identity = self
            .mapping_identity(lineage)
            .map_err(ExistingLineageMapError::Preserved)?;
        let Some(state) = self.uffd.as_deref_mut() else {
            return Ok(None);
        };
        state
            .preflight_tail_extension(
                0,
                self.address_space_id,
                identity.id,
                old_end.as_usize(),
                new_end.as_usize(),
            )
            .map(Some)
            .map_err(ExistingLineageMapError::Preserved)
    }

    pub(super) fn preflight_existing_lineage_head_uffd(
        &mut self,
        lineage: MappingLineage,
        old_start: VirtAddr,
        new_start: VirtAddr,
    ) -> Result<Option<OptionalUffdPlan>, ExistingLineageMapError> {
        let identity = self
            .mapping_identity(lineage)
            .map_err(ExistingLineageMapError::Preserved)?;
        let Some(state) = self.uffd.as_deref_mut() else {
            return Ok(None);
        };
        state
            .preflight_head_extension(
                0,
                self.address_space_id,
                identity.id,
                old_start.as_usize(),
                new_start.as_usize(),
            )
            .map(Some)
            .map_err(ExistingLineageMapError::Preserved)
    }

    pub(super) fn resolve_existing_lineage_uffd(
        &mut self,
        plan: Option<OptionalUffdPlan>,
        published: bool,
    ) {
        let Some(plan) = plan else {
            return;
        };
        let state = self
            .uffd
            .as_deref_mut()
            .expect("armed UFFD lineage-extension plan lost its address-space state");
        if published {
            let wake = state.commit_plan(plan);
            assert!(
                wake.is_empty(),
                "authority-preserving UFFD lineage growth invalidated a live request"
            );
        } else {
            state.abort_plan(plan);
        }
    }

    pub(super) fn map_with_existing_lineage_transaction(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
        locked: bool,
        lineage: MappingLineage,
        uffd_plan: Option<OptionalUffdPlan>,
    ) -> Result<(), ExistingLineageMapError> {
        if let Err(error) = self.validate_region(start, size) {
            self.resolve_existing_lineage_uffd(uffd_plan, false);
            return Err(ExistingLineageMapError::Preserved(error));
        }
        let next_topology_generation = match self.next_topology_generation() {
            Ok(generation) => generation,
            Err(error) => {
                self.resolve_existing_lineage_uffd(uffd_plan, false);
                return Err(ExistingLineageMapError::Preserved(error));
            }
        };
        // If population rollback fails and the new range remains visible,
        // publishing a conservative generation is mandatory and must not
        // introduce a new fallible step after mutation.
        let next_mapping_generation = match self.mapping_identity(lineage) {
            Ok(identity) => identity.generation.next().map_err(mm_error),
            Err(error) => Err(error),
        };
        let next_mapping_generation = match next_mapping_generation {
            Ok(generation) => generation,
            Err(error) => {
                self.resolve_existing_lineage_uffd(uffd_plan, false);
                return Err(ExistingLineageMapError::Preserved(error));
            }
        };

        let area = MemoryArea::new_with_lineage(start, size, flags, backend, lineage);
        if let Err(error) = self
            .areas
            .map_with_limit(area, &mut self.pt, false, MAX_VMA_FRAGMENTS)
        {
            self.resolve_existing_lineage_uffd(uffd_plan, false);
            return Err(ExistingLineageMapError::Preserved(error.into()));
        }
        if locked {
            self.insert_locked_range(start, start + size);
        }
        if populate && let Err(err) = self.populate_area(start, size, flags) {
            let rollback = self.unmap_areas_with_tlb_grace(start, size);
            self.refresh_growdown_starts();
            match rollback {
                Ok(()) => {
                    self.clear_locked_range(start, size);
                    self.resolve_existing_lineage_uffd(uffd_plan, false);
                    return Err(classify_existing_lineage_population_failure(err, false));
                }
                Err(unmap_err) => {
                    warn!(
                        "AddrSpace::map_with_existing_lineage: failed to roll back \
                         {start:?}+{size:#x} after populate error: {unmap_err:?}"
                    );
                    self.resolve_existing_lineage_uffd(uffd_plan, true);
                    self.commit_mapping_generation(lineage, next_mapping_generation);
                    self.commit_topology_generation(next_topology_generation);
                    return Err(classify_existing_lineage_population_failure(err, true));
                }
            }
        }
        self.resolve_existing_lineage_uffd(uffd_plan, true);
        self.commit_mapping_generation(lineage, next_mapping_generation);
        self.commit_topology_generation(next_topology_generation);
        Ok(())
    }

    /// Extends an already identified logical mapping without extending any
    /// userfaultfd registration. `brk` uses this deliberately: a VMA/backend
    /// merge must not grant the new heap tail old range authority.
    pub(crate) fn map_with_existing_lineage(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
        locked: bool,
        lineage: MappingLineage,
    ) -> Result<(), ExistingLineageMapError> {
        self.map_with_existing_lineage_transaction(
            start, size, flags, populate, backend, locked, lineage, None,
        )
    }

    pub(crate) fn extend_mapping_tail_with_existing_lineage(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
        locked: bool,
        lineage: MappingLineage,
    ) -> Result<(), ExistingLineageMapError> {
        let new_end = start
            .checked_add(size)
            .ok_or(ExistingLineageMapError::Preserved(AxError::InvalidInput))?;
        let uffd_plan = self.preflight_existing_lineage_tail_uffd(lineage, start, new_end)?;
        self.map_with_existing_lineage_transaction(
            start, size, flags, populate, backend, locked, lineage, uffd_plan,
        )
    }

    pub(super) fn extend_mapping_head_with_existing_lineage(
        &mut self,
        old_start: VirtAddr,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        backend: Backend,
        locked: bool,
        lineage: MappingLineage,
    ) -> Result<(), ExistingLineageMapError> {
        let uffd_plan = self.preflight_existing_lineage_head_uffd(lineage, old_start, start)?;
        self.map_with_existing_lineage_transaction(
            start, size, flags, false, backend, locked, lineage, uffd_plan,
        )
    }

    /// Stages one fragment under an already reserved lineage without
    /// publishing topology or generation state. The caller must hold the
    /// address-space lock and finish through one of the transaction helpers
    /// below, which owns both commit and rollback.
    pub(crate) fn stage_mapping_fragment(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
        populate: bool,
        backend: Backend,
        locked: bool,
        lineage: MappingLineage,
    ) -> AxResult {
        self.validate_region(start, size)?;
        self.mapping_identity(lineage)?;
        let area = MemoryArea::new_with_lineage(start, size, flags, backend, lineage);
        self.areas
            .map_with_limit(area, &mut self.pt, false, MAX_VMA_FRAGMENTS)?;
        if locked {
            self.insert_locked_range(start, start + size);
        }
        if populate {
            // Leave the fragment visible on failure. The enclosing transaction
            // owns rollback for the complete destination, including any prefix
            // populated before the error.
            self.populate_area(start, size, flags)?;
        }
        Ok(())
    }

    pub(super) fn preflight_transaction_unmap(&mut self, start: VirtAddr, size: usize) -> AxResult {
        self.dontdump_ranges
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        self.areas
            .preflight_unmap(start, size, &self.pt)
            .map_err(Into::into)
    }

    pub(super) fn unmap_areas_with_tlb_grace(
        &mut self,
        start: VirtAddr,
        size: usize,
    ) -> MappingResult {
        self.publish_resident_highwater();
        let retirement =
            self.areas
                .unmap_deferred_with_limit(start, size, &mut self.pt, MAX_VMA_FRAGMENTS)?;
        self.release_swapped_range(start, size);
        let grace = self.synchronize_tlb_after_mutation();
        retirement.release();
        drop(grace);
        self.publish_resident_highwater();
        Ok(())
    }

    pub(super) fn clear_areas_with_tlb_grace(&mut self) -> MappingResult {
        self.publish_resident_highwater();
        let retirement = self.areas.clear_deferred(&mut self.pt)?;
        let swapped: Vec<_> = self.swapped.keys().copied().collect();
        for page in swapped {
            if let Some(entry) = self.swapped.remove(&page) {
                let _ = crate::mm::release(entry);
            }
        }
        let grace = self.synchronize_tlb_after_mutation();
        retirement.release();
        drop(grace);
        self.publish_resident_highwater();
        Ok(())
    }

    pub(super) fn rollback_staged_mapping(
        &mut self,
        start: VirtAddr,
        size: usize,
        lineage: MappingLineage,
    ) -> AxResult {
        if !range_is_owned_by_lineage(&self.areas, lineage, start, size) {
            // Do not retire a sidecar while an out-of-transaction fragment is
            // visible, or unmap a pre-existing mapping that the transaction
            // never owned. The enclosing transaction publishes its prepared
            // topology fence before returning this fail-closed result.
            return Err(AxError::BadState);
        }
        let rollback = self.unmap_areas_with_tlb_grace(start, size);
        if let Err(error) = rollback {
            // The fresh mapping incarnation remains visible. Its generation 1
            // sidecar remains valid; the caller must publish the topology
            // fence before exposing the rollback failure.
            return Err(error.into());
        }
        self.refresh_growdown_starts();
        Self::clear_interval(&mut self.madvise_guard_ranges, start, size);
        Self::clear_interval(&mut self.madvise_hwpoison_ranges, start, size);
        Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
        Self::clear_interval(&mut self.dontfork_ranges, start, size);
        Self::clear_interval_vec(&mut self.dontdump_ranges, start, size);
        self.clear_locked_range(start, size);
        #[cfg(target_arch = "x86_64")]
        self.remove_cet_default_shadow_stack_extents_for_unmap(start, size);
        self.prune_shared_alias_bindings();
        if !self.remove_mapping_lineage_if_unused(lineage) {
            return Err(AxError::BadState);
        }
        Ok(())
    }

    pub(super) fn destroy_remap_destination(&mut self, start: VirtAddr, size: usize) -> AxResult {
        self.unmap_areas_with_tlb_grace(start, size)?;
        self.refresh_growdown_starts();
        let end = start + size;
        self.madvise_free_pages
            .retain(|&page, _| page < start || page >= end);
        Self::clear_interval(&mut self.madvise_guard_ranges, start, size);
        Self::clear_interval(&mut self.madvise_hwpoison_ranges, start, size);
        Self::clear_interval(&mut self.wipe_on_fork_ranges, start, size);
        Self::clear_interval(&mut self.dontfork_ranges, start, size);
        Self::clear_interval_vec(&mut self.dontdump_ranges, start, size);
        self.clear_locked_range(start, size);
        self.prune_shared_alias_bindings();
        Ok(())
    }

    pub(super) fn preflight_remap_uffd(
        &mut self,
        kind: UffdRemapKind,
        fixed: bool,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
    ) -> AxResult<PreparedRemapUffd> {
        let source =
            PageRange::new(source_start.as_usize(), source_size, PAGE_SIZE_4K).map_err(mm_error)?;
        let destination =
            PageRange::new(destination_start.as_usize(), destination_size, PAGE_SIZE_4K)
                .map_err(mm_error)?;
        let AddrSpace {
            address_space_id,
            areas,
            mapping_identities,
            uffd,
            ..
        } = self;
        let Some(state) = uffd.as_deref_mut() else {
            return Ok(PreparedRemapUffd::None);
        };
        let address_space_id = *address_space_id;
        state.preflight_remap(kind, fixed, source, destination, |registration| {
            Self::uffd_snapshot_for_registration(
                address_space_id,
                areas,
                mapping_identities,
                registration,
            )
        })
    }

    pub(super) fn resolve_remap_uffd(
        &mut self,
        prepared: PreparedRemapUffd,
        outcome: RemapUffdOutcome,
    ) -> DeferredUffdWake {
        if let Some(state) = self.uffd.as_deref_mut() {
            state.resolve_remap(prepared, outcome)
        } else {
            assert!(
                matches!(prepared, PreparedRemapUffd::None),
                "armed UFFD remap plan lost its address-space state"
            );
            DeferredUffdWake::empty()
        }
    }

    pub(super) fn finish_failed_mapping_transaction(
        &mut self,
        operation_error: AxError,
        destination_start: VirtAddr,
        destination_size: usize,
        destination_lineage: MappingLineage,
        destination_mutations: &[MappingIdentityMutation],
        destination_changed: bool,
        next_topology_generation: MappingGeneration,
        uffd_plan: PreparedRemapUffd,
        wake: &mut DeferredUffdWake,
    ) -> MappingTransactionFailure {
        let rollback =
            self.rollback_staged_mapping(destination_start, destination_size, destination_lineage);
        let rollback_error = rollback.err();
        let effect = classify_failed_remap_effect(destination_changed, rollback_error.is_some());
        wake.merge(self.resolve_remap_uffd(
            uffd_plan,
            match effect {
                RemapTransactionEffect::Preserved => RemapUffdOutcome::Preserved,
                RemapTransactionEffect::Destructive => RemapUffdOutcome::DestructiveFailure,
            },
        ));
        if destination_changed {
            commit_mapping_identity_mutations(&mut self.mapping_identities, destination_mutations);
        }
        if effect == RemapTransactionEffect::Destructive {
            self.commit_topology_generation(next_topology_generation);
        }
        MappingTransactionFailure {
            error: rollback_error.unwrap_or(operation_error),
            effect,
        }
    }

    pub(super) fn duplicate_mapping_transaction<T>(
        &mut self,
        destination: RemapDestination,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        fragment_limit: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, MappingTransactionFailure> {
        let mut wake = DeferredUffdWake::empty();
        let outcome = (|| {
            self.validate_region(source_start, source_size)
                .map_err(MappingTransactionFailure::preserved)?;
            self.validate_region(destination_start, destination_size)
                .map_err(MappingTransactionFailure::preserved)?;
            if staged_fragments == 0 {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }
            let source_range = VirtAddrRange::from_start_size(source_start, source_size);
            let destination_range =
                VirtAddrRange::from_start_size(destination_start, destination_size);
            if source_range.overlaps(destination_range) {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }
            if !range_is_fully_mapped(&self.areas, source_start, source_size) {
                return Err(MappingTransactionFailure::preserved(AxError::BadAddress));
            }
            self.check_no_seal_overlap(source_start, source_size)
                .map_err(MappingTransactionFailure::preserved)?;

            let replacing = matches!(destination, RemapDestination::Replace);
            if !replacing && !range_is_empty(&self.areas, destination_start, destination_size) {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }
            if replacing {
                self.check_no_seal_overlap(destination_start, destination_size)
                    .map_err(MappingTransactionFailure::preserved)?;
                self.check_no_user_io_pin_overlap(
                    destination_start,
                    destination_size,
                    InvalidationReason::Remap,
                )
                .map_err(MappingTransactionFailure::preserved)?;
            }

            let destination_mutations = if replacing {
                prepare_unmap_mapping_mutations(
                    &self.areas,
                    &self.mapping_identities,
                    destination_start,
                    destination_size,
                )
                .map_err(MappingTransactionFailure::preserved)?
            } else {
                Vec::new()
            };
            let policy = self
                .prepare_remap_policy(source_start, source_size, destination_size)
                .map_err(MappingTransactionFailure::preserved)?;
            let destination_unmaps = [destination_range];
            let unmaps = if replacing {
                destination_unmaps.as_slice()
            } else {
                &[]
            };
            admit_staged_fragments_after_unmaps(
                &self.areas,
                unmaps,
                staged_fragments,
                fragment_limit,
            )
            .map_err(MappingTransactionFailure::preserved)?;
            if replacing {
                self.preflight_transaction_unmap(destination_start, destination_size)
                    .map_err(MappingTransactionFailure::preserved)?;
            }
            let next_topology_generation = self
                .next_topology_generation()
                .map_err(MappingTransactionFailure::preserved)?;
            let destination_lineage = self
                .prepare_fresh_mapping_lineage()
                .map_err(MappingTransactionFailure::preserved)?;
            let uffd_plan = match self.preflight_remap_uffd(
                UffdRemapKind::Duplicate,
                replacing,
                source_start,
                source_size,
                destination_start,
                destination_size,
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    let removed = self.remove_mapping_lineage_if_unused(destination_lineage);
                    debug_assert!(removed);
                    return Err(MappingTransactionFailure::preserved(error));
                }
            };

            let destination_changed = !destination_mutations.is_empty();
            if replacing
                && let Err(error) =
                    self.destroy_remap_destination(destination_start, destination_size)
            {
                let removed = self.remove_mapping_lineage_if_unused(destination_lineage);
                debug_assert!(removed);
                wake.merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Preserved));
                return Err(MappingTransactionFailure::preserved(error));
            }

            let staged = stage(self, destination_lineage);
            match staged {
                Ok(value)
                    if lineage_exactly_covers_range(
                        &self.areas,
                        destination_lineage,
                        destination_start,
                        destination_size,
                    ) =>
                {
                    self.apply_remap_policy(destination_start, &policy);
                    wake.merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Committed));
                    commit_mapping_identity_mutations(
                        &mut self.mapping_identities,
                        &destination_mutations,
                    );
                    self.commit_topology_generation(next_topology_generation);
                    Ok(value)
                }
                Ok(_) => Err(self.finish_failed_mapping_transaction(
                    AxError::BadState,
                    destination_start,
                    destination_size,
                    destination_lineage,
                    &destination_mutations,
                    destination_changed,
                    next_topology_generation,
                    uffd_plan,
                    &mut wake,
                )),
                Err(operation_error) => Err(self.finish_failed_mapping_transaction(
                    operation_error,
                    destination_start,
                    destination_size,
                    destination_lineage,
                    &destination_mutations,
                    destination_changed,
                    next_topology_generation,
                    uffd_plan,
                    &mut wake,
                )),
            }
        })();
        LockExternalUffdOutcome::new(outcome, wake)
    }

    pub(super) fn move_mapping_transaction<T>(
        &mut self,
        destination: RemapDestination,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        fragment_limit: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, MappingTransactionFailure> {
        let mut wake = DeferredUffdWake::empty();
        let outcome = (|| {
            self.validate_region(source_start, source_size)
                .map_err(MappingTransactionFailure::preserved)?;
            self.validate_region(destination_start, destination_size)
                .map_err(MappingTransactionFailure::preserved)?;
            if staged_fragments == 0 {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }
            self.check_no_user_io_pin_overlap(source_start, source_size, InvalidationReason::Remap)
                .map_err(MappingTransactionFailure::preserved)?;
            self.check_no_seal_overlap(source_start, source_size)
                .map_err(MappingTransactionFailure::preserved)?;
            let source_range = VirtAddrRange::from_start_size(source_start, source_size);
            let destination_range =
                VirtAddrRange::from_start_size(destination_start, destination_size);
            if source_range.overlaps(destination_range) {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }

            let replacing = matches!(destination, RemapDestination::Replace);
            if !replacing && !range_is_empty(&self.areas, destination_start, destination_size) {
                return Err(MappingTransactionFailure::preserved(AxError::InvalidInput));
            }
            if replacing {
                self.check_no_seal_overlap(destination_start, destination_size)
                    .map_err(MappingTransactionFailure::preserved)?;
                self.check_no_user_io_pin_overlap(
                    destination_start,
                    destination_size,
                    InvalidationReason::Remap,
                )
                .map_err(MappingTransactionFailure::preserved)?;
            }

            if !range_is_fully_mapped(&self.areas, source_start, source_size) {
                return Err(MappingTransactionFailure::preserved(AxError::BadAddress));
            }
            let destination_mutations = if replacing {
                prepare_unmap_mapping_mutations(
                    &self.areas,
                    &self.mapping_identities,
                    destination_start,
                    destination_size,
                )
                .map_err(MappingTransactionFailure::preserved)?
            } else {
                Vec::new()
            };
            let replacement_success_ranges = [destination_range, source_range];
            let source_success_ranges = [source_range];
            let success_ranges = if replacing {
                replacement_success_ranges.as_slice()
            } else {
                source_success_ranges.as_slice()
            };
            let success_mutations = prepare_unmap_mapping_mutations_for_ranges(
                &self.areas,
                &self.mapping_identities,
                success_ranges,
            )
            .map_err(MappingTransactionFailure::preserved)?;
            if success_mutations.is_empty() {
                return Err(MappingTransactionFailure::preserved(AxError::BadAddress));
            }

            let policy = self
                .prepare_remap_policy(source_start, source_size, destination_size)
                .map_err(MappingTransactionFailure::preserved)?;
            let replacement_destination_unmaps = [destination_range];
            let destination_unmaps = if replacing {
                replacement_destination_unmaps.as_slice()
            } else {
                &[]
            };
            admit_staged_fragments_after_unmaps(
                &self.areas,
                destination_unmaps,
                staged_fragments,
                fragment_limit,
            )
            .map_err(MappingTransactionFailure::preserved)?;
            admit_staged_fragments_after_unmaps(
                &self.areas,
                success_ranges,
                staged_fragments,
                fragment_limit,
            )
            .map_err(MappingTransactionFailure::preserved)?;
            if replacing {
                self.preflight_transaction_unmap(destination_start, destination_size)
                    .map_err(MappingTransactionFailure::preserved)?;
            }
            self.preflight_transaction_unmap(source_start, source_size)
                .map_err(MappingTransactionFailure::preserved)?;
            let next_topology_generation = self
                .next_topology_generation()
                .map_err(MappingTransactionFailure::preserved)?;
            let destination_lineage = self
                .prepare_fresh_mapping_lineage()
                .map_err(MappingTransactionFailure::preserved)?;
            let uffd_plan = match self.preflight_remap_uffd(
                UffdRemapKind::Move,
                replacing,
                source_start,
                source_size,
                destination_start,
                destination_size,
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    let removed = self.remove_mapping_lineage_if_unused(destination_lineage);
                    debug_assert!(removed);
                    return Err(MappingTransactionFailure::preserved(error));
                }
            };

            let destination_changed = !destination_mutations.is_empty();
            if replacing
                && let Err(error) =
                    self.destroy_remap_destination(destination_start, destination_size)
            {
                let removed = self.remove_mapping_lineage_if_unused(destination_lineage);
                debug_assert!(removed);
                wake.merge(self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Preserved));
                return Err(MappingTransactionFailure::preserved(error));
            }

            let staged = stage(self, destination_lineage);
            let operation: AxResult<T> = match staged {
                Ok(value)
                    if lineage_exactly_covers_range(
                        &self.areas,
                        destination_lineage,
                        destination_start,
                        destination_size,
                    ) =>
                {
                    self.apply_remap_policy(destination_start, &policy);
                    let source_commit = self.unmap_areas_with_tlb_grace(source_start, source_size);
                    match source_commit {
                        Ok(()) => {
                            self.refresh_growdown_starts();
                            Self::clear_interval(
                                &mut self.wipe_on_fork_ranges,
                                source_start,
                                source_size,
                            );
                            Self::clear_interval(
                                &mut self.dontfork_ranges,
                                source_start,
                                source_size,
                            );
                            Self::clear_interval_vec(
                                &mut self.dontdump_ranges,
                                source_start,
                                source_size,
                            );
                            self.clear_locked_range(source_start, source_size);
                            wake.merge(
                                self.resolve_remap_uffd(uffd_plan, RemapUffdOutcome::Committed),
                            );
                            commit_mapping_identity_mutations(
                                &mut self.mapping_identities,
                                &success_mutations,
                            );
                            self.commit_topology_generation(next_topology_generation);
                            return Ok(value);
                        }
                        Err(error) => Err(error.into()),
                    }
                }
                Ok(_) => Err(AxError::BadState),
                Err(error) => Err(error),
            };

            Err(self.finish_failed_mapping_transaction(
                operation
                    .err()
                    .expect("failed remap operation lost its error"),
                destination_start,
                destination_size,
                destination_lineage,
                &destination_mutations,
                destination_changed,
                next_topology_generation,
                uffd_plan,
                &mut wake,
            ))
        })();
        LockExternalUffdOutcome::new(outcome, wake)
    }
}
