//! Eviction fences, CET shadow-stack ownership and prepared protect/remap plans.

use super::*;

/// One in-flight file-cache page retirement in this address space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileEvictionFenceKey {
    pub cache: axfs::CachedFileIdentity,
    pub page_number: u32,
    pub generation: u64,
}

/// Internal, lock-external retry state for an alias mutation that encountered
/// a cache-page eviction fence.  It is never translated to a userspace errno:
/// callers drop the `AddrSpace` mutex, wait for this exact cache's terminal
/// completion edge, then revalidate the VMA and retry their whole operation.
#[derive(Clone)]
pub(crate) struct FileEvictionRetry {
    pub(crate) cache: axfs::CachedFile,
    pub(crate) key: FileEvictionFenceKey,
    pub(super) observed_epoch: u64,
}

impl FileEvictionRetry {
    pub(super) fn new(cache: axfs::CachedFile, key: FileEvictionFenceKey) -> Self {
        let observed_epoch = cache.eviction_completion_epoch();
        Self {
            cache,
            key,
            observed_epoch,
        }
    }

    pub(crate) fn wait(self) -> AxResult {
        self.cache
            .wait_for_eviction_completion(self.observed_epoch)
            .map_err(|_| AxError::Interrupted)
    }
}

/// Kernel-only ownership for one automatically allocated CET shadow stack.
/// Explicit `map_shadow_stack(2)` mappings never enter this registry.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CetDefaultShadowStackExtent {
    pub start: VirtAddr,
    pub size: usize,
}

impl CetDefaultShadowStackExtent {
    pub(super) fn end(self) -> Option<VirtAddr> {
        self.start.checked_add(self.size)
    }
}

/// `start` and `size` retain the first extent for the small number of
/// diagnostic callers that only need a normal, contiguous automatic stack.
/// Cleanup and vfork ownership always use `extents`: peer VMA operations may
/// split an automatic stack, move an interior range, or duplicate a range.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CetDefaultShadowStackOwner {
    pub task_id: u32,
    pub start: VirtAddr,
    pub size: usize,
    pub extents: Vec<CetDefaultShadowStackExtent>,
    pub ownership: CetDefaultShadowStackOwnership,
}

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CetDefaultShadowStackOwnership {
    Owned,
    Borrowed,
}

// `AddrSpace` is constructed by value during early boot and fork preparation.
// Keep the long-lived object below one base page so fixed-capacity policy
// sidecars cannot silently turn those call chains into large stack frames.
const _: () = assert!(core::mem::size_of::<AddrSpace>() <= PAGE_SIZE_4K);

/// The generic, testable core of one linear protection transaction.
///
/// This value owns the only mutable access to both the area tree and its page
/// table until it is either committed or dropped.
pub(super) struct PreparedAreaProtect<'a, B: memory_set::MappingBackend> {
    pub(super) areas: &'a mut MemorySet<B>,
    pub(super) page_table: &'a mut B::PageTable,
    pub(super) start: B::Addr,
    pub(super) end: B::Addr,
    pub(super) ranges: Vec<PreparedProtectRange<B::Addr, B::Flags>>,
    pub(super) max_areas: usize,
}

/// One already-admitted portion of a protection transaction. Ranges are
/// disjoint and cover the complete transaction interval.
#[derive(Clone, Copy)]
pub(super) struct PreparedProtectRange<A, F> {
    pub(super) start: A,
    pub(super) end: A,
    pub(super) flags: F,
}

impl<'a, B: memory_set::MappingBackend> PreparedAreaProtect<'a, B> {
    pub(super) fn flags_at(&self, address: B::Addr) -> B::Flags {
        self.ranges
            .iter()
            .find(|range| range.start <= address && address < range.end)
            .expect("prepared protection ranges cover every affected VMA")
            .flags
    }

    pub(super) fn segments(
        &self,
    ) -> impl Iterator<Item = (&MemoryArea<B>, B::Addr, B::Addr, B::Flags)> + '_ {
        let start = self.start;
        let end = self.end;
        self.areas.iter().filter_map(move |area| {
            let affected_start = area.start().max(start);
            let affected_end = area.end().min(end);
            (affected_start < affected_end).then(|| {
                (
                    area,
                    affected_start,
                    affected_end,
                    self.flags_at(affected_start),
                )
            })
        })
    }

    pub(super) fn commit(self) -> MappingResult<&'a mut MemorySet<B>> {
        let Self {
            areas,
            page_table,
            start,
            end,
            ranges,
            max_areas,
        } = self;
        areas.protect_with_limit(
            start,
            end.sub_addr(start),
            |affected_start, _| {
                Some(
                    ranges
                        .iter()
                        .find(|range| range.start <= affected_start && affected_start < range.end)
                        .expect("prepared protection ranges cover every affected VMA")
                        .flags,
                )
            },
            page_table,
            max_areas,
        )?;
        Ok(areas)
    }
}

/// Commits the main area transaction before handing a prepared sidecar back
/// to its caller for publication.
///
/// The synchronization callback runs after the page-table attempt on both the
/// success and failure paths. If the main transaction fails, `?` drops the
/// sidecar in this function; an RAII sidecar can therefore abort its own
/// preflight authority before the error escapes. On success, the caller gets
/// the still-armed sidecar back and may publish it only after any infallible
/// main-MM bookkeeping that must precede sidecar visibility.
pub(super) fn commit_area_before_sidecar<'a, B, S>(
    transaction: PreparedAreaProtect<'a, B>,
    sidecar: S,
    synchronize: impl FnOnce(),
) -> MappingResult<(&'a mut MemorySet<B>, S)>
where
    B: memory_set::MappingBackend,
{
    let areas = transaction.commit();
    synchronize();
    let areas = areas?;
    Ok((areas, sidecar))
}

#[derive(Clone, Copy)]
pub(super) struct ProjectedProtectPiece<'a, B: memory_set::MappingBackend> {
    pub(super) area: &'a MemoryArea<B>,
    pub(super) start: B::Addr,
    pub(super) end: B::Addr,
    pub(super) flags: B::Flags,
}

pub(super) struct ProjectedProtectRun<'a, B: memory_set::MappingBackend> {
    pub(super) left_area: &'a MemoryArea<B>,
    pub(super) start: B::Addr,
    pub(super) end: B::Addr,
    pub(super) flags: B::Flags,
}

pub(super) fn projected_protect_pieces_share_structure<B: memory_set::MappingBackend>(
    left: &ProjectedProtectPiece<'_, B>,
    right: &ProjectedProtectPiece<'_, B>,
) -> bool {
    left.end == right.start
        && left.flags == right.flags
        && left.area.lineage() == right.area.lineage()
}

pub(super) fn projected_protect_pieces_merge<B: memory_set::MappingBackend>(
    left: &ProjectedProtectPiece<'_, B>,
    right: &ProjectedProtectPiece<'_, B>,
) -> bool {
    projected_protect_pieces_share_structure(left, right)
        && memory_set::MappingBackend::can_merge(left.area.backend(), right.area.backend())
}

/// One immutable, pre-change VMA view in a prepared protection transaction.
///
/// The full area bounds identify the VMA that future policy hooks must inspect;
/// the affected bounds identify the subrange this transaction will change.
/// Neither the view nor its backend reference permits mutation.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct PreparedProtectSegment<'a> {
    pub(super) area: &'a MemoryArea<Backend>,
    pub(super) affected: VirtAddrRange,
    pub(super) new_flags: MappingFlags,
}

#[allow(dead_code)]
impl<'a> PreparedProtectSegment<'a> {
    #[cfg(test)]
    pub(crate) const fn for_test(area: &'a MemoryArea<Backend>, affected: VirtAddrRange) -> Self {
        Self {
            area,
            affected,
            new_flags: area.flags(),
        }
    }

    pub(crate) const fn area_start(self) -> VirtAddr {
        self.area.start()
    }

    pub(crate) const fn area_end(self) -> VirtAddr {
        self.area.end()
    }

    pub(crate) const fn affected(self) -> VirtAddrRange {
        self.affected
    }

    pub(crate) const fn flags(self) -> MappingFlags {
        self.area.flags()
    }

    pub(crate) const fn new_flags(self) -> MappingFlags {
        self.new_flags
    }

    pub(crate) const fn backend(self) -> &'a Backend {
        self.area.backend()
    }

    pub(crate) fn file_mapping(self) -> Option<&'a FileMappingLease> {
        self.area.backend().file_mapping()
    }

    pub(crate) fn area_file_offset(self) -> Option<u64> {
        self.file_mapping()?.file_offset_at(self.area.start())
    }

    pub(crate) fn affected_file_offset(self) -> Option<u64> {
        self.file_mapping()?.file_offset_at(self.affected.start)
    }
}

/// Linear admission for one fully preflighted `mprotect` transaction.
///
/// Construction validates every target VMA without changing the area tree,
/// page table, pin state, or backend state. Dropping the value aborts with no
/// side effects; only [`Self::commit`] starts the existing transactional
/// split/protect/merge path.
#[must_use = "a prepared protection must be committed explicitly or dropped to abort"]
pub(crate) struct PreparedProtect<'a> {
    pub(super) transaction: PreparedAreaProtect<'a, Backend>,
    pub(super) growdown_starts: &'a mut BTreeSet<VirtAddr>,
    pub(super) topology_generation: &'a mut MappingGeneration,
    pub(super) next_topology_generation: MappingGeneration,
    pub(super) tlb: &'a TlbState,
    pub(super) mapping_identities: &'a mut MappingIdentityIndex,
    pub(super) mapping_mutations: Vec<MappingIdentityMutation>,
    pub(super) uffd_mutation: Option<PreparedUffdMutation<'a>>,
    pub(super) synchronize_instruction_stream: bool,
}

/// Resources reserved before pkey protection splits a resident huge leaf.
/// Keeping these tables outside the VMA transaction means allocation failure
/// leaves both the PTEs and mapping metadata untouched.
pub(crate) struct PreparedPkeyDemotion {
    pub(super) leaves: Vec<PreparedPkeyLeaf>,
    pub(super) start: VirtAddr,
    pub(super) end: VirtAddr,
}

pub(super) struct PreparedPkeyLeaf {
    pub(super) vaddr: VirtAddr,
    pub(super) paddr: PhysAddr,
    pub(super) size: PageSize,
    pub(super) cow_backing: bool,
    pub(super) tables: PreparedPageTableFrames,
}

impl PreparedPkeyDemotion {
    pub(super) fn prepare_table_error(error: PrepareTableFramesError) -> AxError {
        match error {
            PrepareTableFramesError::NoMemory => AxError::NoMemory,
            PrepareTableFramesError::TooMany { .. } => AxError::BadState,
        }
    }

    pub(crate) fn commit(&mut self, pt: &mut PageTable) -> AxResult {
        // Ownership registration preserves units even if a later registration
        // fails: the unchanged huge PTE can retire through the same registry.
        // Finish all fallible registration before publishing any new geometry.
        for leaf in &self.leaves {
            if leaf.cow_backing {
                backend::register_demoted_huge_backing(leaf.paddr, leaf.size)?;
            }
        }
        let mut cursor = pt.cursor();
        for leaf in &mut self.leaves {
            cursor
                .demote_leaf_for_range_prepared(
                    leaf.vaddr.max(self.start),
                    (leaf.vaddr + leaf.size as usize).min(self.end) - leaf.vaddr.max(self.start),
                    &mut leaf.tables,
                )
                .map_err(AxError::from)?;
        }
        Ok(())
    }

    pub(crate) fn apply_key(&self, pt: &mut PageTable, key: Pkey) -> AxResult {
        let mut cursor = pt.cursor();
        for leaf in &self.leaves {
            let address = leaf.vaddr.max(self.start);
            let end = (leaf.vaddr + leaf.size as usize).min(self.end);
            cursor
                .set_pkey_region(address, end - address, key)
                .map_err(AxError::from)?;
        }
        Ok(())
    }
}

pub(super) enum RemapDestination {
    Empty,
    Replace,
}

/// Fixed-remap failure classification used by syscall glue to invalidate
/// per-range policy only when the transaction changed visible mappings.
#[derive(Debug)]
pub(crate) enum ReplaceMappingError {
    AddressSpacePreserved(AxError),
    AddressSpaceChanged(AxError),
}

impl ReplaceMappingError {
    pub(crate) const fn mapping_changed(&self) -> bool {
        matches!(self, Self::AddressSpaceChanged(_))
    }

    pub(crate) fn into_error(self) -> AxError {
        match self {
            Self::AddressSpacePreserved(error) | Self::AddressSpaceChanged(error) => error,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ExistingLineageMapError {
    Preserved(AxError),
    Published(AxError),
}

impl ExistingLineageMapError {
    pub(crate) const fn published(self) -> bool {
        matches!(self, Self::Published(_))
    }

    pub(crate) const fn into_error(self) -> AxError {
        match self {
            Self::Preserved(error) | Self::Published(error) => error,
        }
    }
}

/// Prepared non-MM work coupled to one installed MAP_FIXED replacement.
/// Rollback must be allocation-free and restore the participant's exact
/// pre-install authority before the memory-set guard restores old PTEs.
pub(crate) trait FixedReplacementParticipant {
    /// Runs after every fallible MM admission has completed but before the
    /// incoming PTE/VMA becomes reachable.  Participants use this edge for
    /// topology identities (for example XOL) which must not describe the new
    /// mapping even for one runnable peer.
    fn before_install(&mut self, aspace: &mut AddrSpace) -> AxResult;
    fn commit(&mut self, aspace: &mut AddrSpace) -> AxResult;
    /// Undo every participant effect while the incoming VMA/PTEs are still
    /// live.  The fixed-replacement guard will restore old leaves immediately
    /// afterwards regardless of this result; an error is reported rather
    /// than silently converted into a deferred cleanup.
    fn rollback(&mut self, aspace: &mut AddrSpace) -> AxResult;
}

pub(super) const fn classify_existing_lineage_population_failure(
    error: AxError,
    rollback_failed: bool,
) -> ExistingLineageMapError {
    if rollback_failed {
        ExistingLineageMapError::Published(error)
    } else {
        ExistingLineageMapError::Preserved(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RemapTransactionEffect {
    Preserved,
    Destructive,
}

pub(super) const fn classify_failed_remap_effect(
    destination_changed: bool,
    rollback_failed: bool,
) -> RemapTransactionEffect {
    if destination_changed || rollback_failed {
        RemapTransactionEffect::Destructive
    } else {
        RemapTransactionEffect::Preserved
    }
}

#[derive(Debug)]
pub(super) struct MappingTransactionFailure {
    pub(super) error: AxError,
    pub(super) effect: RemapTransactionEffect,
}

impl MappingTransactionFailure {
    pub(super) fn preserved(error: AxError) -> Self {
        Self {
            error,
            effect: RemapTransactionEffect::Preserved,
        }
    }

    pub(super) fn into_replace_error(self) -> ReplaceMappingError {
        match self.effect {
            RemapTransactionEffect::Preserved => {
                ReplaceMappingError::AddressSpacePreserved(self.error)
            }
            RemapTransactionEffect::Destructive => {
                ReplaceMappingError::AddressSpaceChanged(self.error)
            }
        }
    }
}

pub(super) struct RelativePolicyRange {
    pub(super) offset: usize,
    pub(super) size: usize,
}

pub(super) struct RemapPolicyPlan {
    pub(super) growdown: bool,
    pub(super) wipe_on_fork: Vec<RelativePolicyRange>,
    pub(super) dontfork: Vec<RelativePolicyRange>,
    pub(super) dontdump: Vec<RelativePolicyRange>,
    pub(super) locked: Vec<RelativePolicyRange>,
}

impl PreparedProtect<'_> {
    /// Iterates the exact pre-change VMAs in increasing virtual-address order.
    #[allow(dead_code)]
    pub(crate) fn segments(&self) -> impl Iterator<Item = PreparedProtectSegment<'_>> + '_ {
        self.transaction
            .segments()
            .map(
                |(area, affected_start, affected_end, flags)| PreparedProtectSegment {
                    area,
                    affected: VirtAddrRange::new(affected_start, affected_end),
                    new_flags: flags,
                },
            )
    }

    /// Commits the already-preflighted request through MemorySet's staged
    /// split/protect/rollback/merge transaction.
    pub(crate) fn commit(self) -> AxResult<DeferredUffdWake> {
        let Self {
            transaction,
            growdown_starts,
            topology_generation,
            next_topology_generation,
            tlb,
            mapping_identities,
            mapping_mutations,
            uffd_mutation,
            synchronize_instruction_stream,
        } = self;
        let (areas, uffd_mutation) =
            commit_area_before_sidecar(transaction, uffd_mutation, || {
                if synchronize_instruction_stream {
                    let _ = super::super::synchronize_tlb_and_icache();
                } else {
                    let _ = tlb.synchronize_after_mutation();
                }
            })?;
        Self::refresh_growdown_starts(areas, growdown_starts);
        let wake = uffd_mutation.map_or_else(DeferredUffdWake::empty, |mutation| mutation.commit());
        commit_mapping_identity_mutations(mapping_identities, &mapping_mutations);
        *topology_generation = next_topology_generation;
        Ok(wake)
    }

    /// Commits pkey protection after prepared huge-leaf tables have been
    /// published. The reservation is consumed before MemorySet performs its
    /// now-preflighted PTE protection pass, and no allocation occurs in the
    /// interval.
    pub(crate) fn commit_with_pkey_demotion(
        self,
        demotion: &mut PreparedPkeyDemotion,
        key: Pkey,
    ) -> AxResult<DeferredUffdWake> {
        demotion.commit(self.transaction.page_table)?;
        demotion.apply_key(self.transaction.page_table, key)?;
        self.commit()
    }

    pub(super) fn refresh_growdown_starts(
        areas: &MemorySet<Backend>,
        growdown_starts: &mut BTreeSet<VirtAddr>,
    ) {
        let starts: Vec<_> = growdown_starts.iter().copied().collect();
        growdown_starts.clear();
        for start in starts {
            if areas.find(start).is_some_and(|area| area.start() == start) {
                growdown_starts.insert(start);
            }
        }
    }
}
