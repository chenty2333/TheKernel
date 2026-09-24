//! AddrSpace: population, discard, madvise, unmap and direct data access.

use super::*;

impl AddrSpace {
    pub(crate) fn duplicate_mapping_into_empty_transaction<T>(
        &mut self,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, AxError> {
        self.duplicate_mapping_transaction(
            RemapDestination::Empty,
            source_start,
            source_size,
            destination_start,
            destination_size,
            staged_fragments,
            MAX_VMA_FRAGMENTS,
            stage,
        )
        .map_err(|failure| failure.error)
    }

    pub(crate) fn replace_and_duplicate_mapping_transaction<T>(
        &mut self,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, ReplaceMappingError> {
        self.duplicate_mapping_transaction(
            RemapDestination::Replace,
            source_start,
            source_size,
            destination_start,
            destination_size,
            staged_fragments,
            MAX_VMA_FRAGMENTS,
            stage,
        )
        .map_err(MappingTransactionFailure::into_replace_error)
    }

    pub(crate) fn move_mapping_into_empty_transaction<T>(
        &mut self,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, AxError> {
        self.move_mapping_transaction(
            RemapDestination::Empty,
            source_start,
            source_size,
            destination_start,
            destination_size,
            staged_fragments,
            MAX_VMA_FRAGMENTS,
            stage,
        )
        .map_err(|failure| failure.error)
    }

    pub(crate) fn replace_and_move_mapping_transaction<T>(
        &mut self,
        source_start: VirtAddr,
        source_size: usize,
        destination_start: VirtAddr,
        destination_size: usize,
        staged_fragments: usize,
        stage: impl FnOnce(&mut Self, MappingLineage) -> AxResult<T>,
    ) -> LockExternalUffdOutcome<T, ReplaceMappingError> {
        self.move_mapping_transaction(
            RemapDestination::Replace,
            source_start,
            source_size,
            destination_start,
            destination_size,
            staged_fragments,
            MAX_VMA_FRAGMENTS,
            stage,
        )
        .map_err(MappingTransactionFailure::into_replace_error)
    }

    /// Populates the area with physical frames, returning false if the area
    /// contains unmapped area.
    pub fn populate_area(
        &mut self,
        mut start: VirtAddr,
        size: usize,
        access_flags: MappingFlags,
    ) -> AxResult {
        self.validate_region(start, size)?;
        // All generic COW fault/populate users operate on 4 KiB spans.  Make
        // that representation explicit here, rather than relying on each
        // usercopy, process_vm, or task-fault caller to remember the huge-PMD
        // boundary rule.
        self.ensure_4k_granularity(start, size)?;
        let end = start + size;

        while let Some(area) = self.areas.find(start) {
            let area_end = area.end();
            let range = VirtAddrRange::new(start, area_end.min(end));
            let area_flags = area.flags();
            let resident_before = self.pt.mapped_bytes(range.start, range.size());
            let outcome =
                area.backend()
                    .populate(range, area_flags, access_flags, &mut self.pt.cursor());
            let result = outcome.finish(self);
            // A backend may have published a valid executable prefix before a
            // later page fails, so synchronize before propagating the error.
            synchronize_executable_publication(area_flags);
            // Usercopy also visits already-resident pages. Avoid walking every
            // VMA and PTE in the mm when this range did not gain resident pages.
            // Check before propagating errors: a populated prefix still counts
            // toward the peak even if a later page failed.
            let resident_after = self.pt.mapped_bytes(range.start, range.size());
            if !matches!((resident_before, resident_after), (Ok(before), Ok(after)) if after <= before)
            {
                self.publish_resident_highwater();
            }
            result?;
            start = area_end;
            if !start.is_aligned_4k() {
                return Err(AxError::BadAddress);
            }
            if start >= end {
                break;
            }
        }

        if start < end {
            // If the area is not fully mapped, we return ENOMEM.
            ax_bail!(NoMemory);
        }

        Ok(())
    }

    /// Removes only resident PTEs backed by one revoked external PCI object.
    /// The VMA metadata deliberately remains: a later access reaches the
    /// dead external backend and faults terminally instead of silently
    /// rebinding a reused BAR page.  Scanning the current area tree makes
    /// split, fork, mremap and partial-unmap exact without stale range
    /// registrations.
    pub(crate) fn revoke_external_shared_pages(&mut self, pages: &Arc<SharedPages>) {
        let mut cursor_addr = self.base();
        let mut revoked = false;
        let mut cursor = self.pt.cursor();
        loop {
            let next = self
                .areas
                .iter()
                .find(|area| area.end() > cursor_addr)
                .map(|area| {
                    (
                        area.end(),
                        area.backend()
                            .shared_pages()
                            .is_some_and(|candidate| Arc::ptr_eq(candidate, pages))
                            .then(|| (area.start(), area.size(), area.backend().clone())),
                    )
                });
            let Some((next_addr, target)) = next else {
                break;
            };
            cursor_addr = next_addr;
            let Some((start, size, backend)) = target else {
                continue;
            };
            let Some(range) = VirtAddrRange::try_from_start_size(start, size) else {
                continue;
            };
            // SharedBackend::unmap drains present leaves without changing the
            // VMA. It cannot release PCI pages because SharedPages owns only
            // the lease, not the physical memory.
            if backend.unmap(range, &mut cursor).is_ok() {
                revoked = true;
            };
        }
        drop(cursor);
        if revoked {
            drop(self.synchronize_tlb_after_mutation());
        }
    }

    pub fn discard_pages(&mut self, mut start: VirtAddr, size: usize) -> AxResult {
        self.validate_region(start, size)?;
        self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Discard)?;
        self.ensure_4k_granularity(start, size)?;
        self.publish_resident_highwater();
        let next_generation = self.next_topology_generation()?;
        // Backend discard can make partial progress before reporting a later
        // hole or backend error. Advance the legacy address-space admission
        // fence first, but deliberately keep per-lineage VMA generations
        // stable: residency is not a mapping-contract change.
        self.commit_topology_generation(next_generation);
        let discard_start = start;
        let end = start + size;

        let retirement_capacity = self
            .areas
            .iter_overlapping(VirtAddrRange::new(start, end))
            .count();
        let mut retired = Vec::new();
        retired
            .try_reserve_exact(retirement_capacity)
            .map_err(|_| AxError::NoMemory)?;
        let result = {
            let mut modify = self.pt.cursor();
            (|| {
                while let Some(area) = self.areas.find(start) {
                    if area.start() > start {
                        break;
                    }

                    let range = VirtAddrRange::new(start, area.end().min(end));
                    retired.push(area.backend().unmap(range, &mut modify)?);
                    start = range.end;
                    if start >= end {
                        break;
                    }
                }

                if start < end {
                    ax_bail!(NoMemory);
                }

                Ok(())
            })()
        };
        // A software swap PTE is already non-present, so backend `unmap`
        // cannot see it.  Discard is nevertheless an ownership drop.
        self.release_swapped_range(discard_start, start.sub_addr(discard_start));
        self.madvise_free_pages
            .retain(|&page, _| page < discard_start || page >= end);
        let grace = self.synchronize_tlb_after_mutation();
        drop(retired);
        drop(grace);
        self.publish_resident_highwater();
        result
    }

    /// Moves already-resident leaves to the cold end of the local reclaim
    /// policy without faulting or detaching their backing frames.
    ///
    /// x86 records the accessed bit in the PTE rather than in
    /// [`MappingFlags`]. Reinstalling the exact translation clears that
    /// hardware-owned state while retaining all software permissions and the
    /// physical frame. This is the only safe anonymous/shmem COLD operation
    /// available before swap exists: unmapping a COW leaf would release its
    /// sole frame, and unmapping a shared leaf would not make its backing
    /// reclaimable. A later PAGEOUT may reclaim file-cache pages, but has the
    /// Linux no-swap outcome for these retained anonymous leaves.
    pub(crate) fn cold_resident_pages(&mut self, range: VirtAddrRange) -> AxResult<usize> {
        self.validate_region(range.start, range.size())?;
        // COLD/PAGEOUT walk individual translations in order to clear the
        // hardware accessed state.  Do not let a range which starts or ends
        // inside a collapsed shared/file PMD retain that compound leaf: the
        // next partial mprotect/munmap/mremap must see the same 4 KiB
        // geometry, and ensure_4k_granularity performs the alias-preserving
        // demotion before this page-by-page walk observes any PTE.
        self.ensure_4k_granularity(range.start, range.size())?;
        let mut cursor = range.start;
        let mut cooled = 0usize;
        let mut changed = false;
        let result = {
            let mut pt = self.pt.cursor();
            (|| {
                while cursor < range.end {
                    match pt.query(cursor) {
                        Ok((paddr, flags, page_size)) => {
                            let leaf_start = cursor.align_down(page_size);
                            let leaf_end = leaf_start + page_size as usize;
                            let leaf_paddr = paddr.align_down(page_size);
                            pt.remap(leaf_start, leaf_paddr, flags)
                                .map_err(|_| AxError::BadAddress)?;
                            changed = true;
                            let covered_end = leaf_end.min(range.end);
                            cooled = cooled
                                .checked_add(covered_end.sub_addr(cursor))
                                .ok_or(AxError::InvalidInput)?;
                            cursor = covered_end;
                        }
                        Err(PagingError::NotMapped) => cursor += PAGE_SIZE_4K,
                        Err(_) => return Err(AxError::BadAddress),
                    }
                }
                if changed {
                    pt.flush();
                }
                Ok(())
            })()
        };
        if changed {
            // The replacement PTEs retain their frame ownership, but remote
            // CPUs may retain an accessed translation. Finish the required
            // targeted invalidation before exposing the cold state.
            drop(self.synchronize_tlb_after_mutation());
        }
        result?;
        Ok(cooled)
    }

    /// Implements anonymous MADV_FREE as a lazy-free generation, rather than
    /// pretending that advice completed while retaining an indistinguishable
    /// ordinary writable leaf.  Only resident private-anonymous pages enter
    /// the ledger; absent pages carry no data to reclaim.  Write protection
    /// gives the fault path one precise cancellation edge before a subsequent
    /// userspace store can make the page dirty again.
    pub(crate) fn mark_madvise_free(&mut self, start: VirtAddr, size: usize) -> AxResult<()> {
        self.validate_region(start, size)?;
        self.ensure_4k_granularity(start, size)?;
        let end = start + size;
        let generation = self
            .next_madvise_free_generation
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        let mut staged = Vec::new();
        staged
            .try_reserve_exact(size / PAGE_SIZE_4K)
            .map_err(|_| AxError::NoMemory)?;
        let mut page = start;
        while page < end {
            let area = self.areas.find(page).ok_or(AxError::NoMemory)?;
            if area.start() > page || !area.backend().is_private_anonymous() {
                return Err(AxError::InvalidInput);
            }
            if let Ok((paddr, flags, PageSize::Size4K)) = self.pt.query(page)
                && area.backend().swap_reclaimable(paddr)
            {
                // Only an exclusively owned anonymous leaf may be protected
                // and later restored directly. Shared fork-COW leaves retain
                // the normal backend write-fault/copy path.
                staged.push((page, paddr, flags));
            }
            page += PAGE_SIZE_4K;
        }
        let mut changed = false;
        let mut cursor = self.pt.cursor();
        for (index, &(page, paddr, flags)) in staged.iter().enumerate() {
            let protected = flags & !MappingFlags::WRITE;
            if protected != flags {
                if cursor.remap(page, paddr, protected).is_err() {
                    // No ledger entry is published until every PTE is
                    // protected. Restore the already changed prefix before
                    // reporting failure so no ordinary writable page is
                    // accidentally left behind a stale lazy-free contract.
                    for &(old_page, old_paddr, old_flags) in &staged[..index] {
                        cursor
                            .remap(old_page, old_paddr, old_flags)
                            .expect("preflighted MADV_FREE PTE rollback must succeed");
                    }
                    if changed {
                        cursor.flush();
                        drop(cursor);
                        drop(self.synchronize_tlb_after_mutation());
                    }
                    return Err(AxError::BadAddress);
                }
                changed = true;
            }
        }
        // This is the single publication point: either every changed PTE is
        // protected and every resident page has a generation, or neither
        // state becomes visible to the reclaim/fault paths.
        for &(page, paddr, flags) in &staged {
            self.madvise_free_pages.insert(
                page,
                LazyFreePage {
                    generation,
                    paddr,
                    restore_flags: flags,
                },
            );
        }
        self.next_madvise_free_generation = generation;
        if changed {
            cursor.flush();
            drop(cursor);
            drop(self.synchronize_tlb_after_mutation());
        }
        Ok(())
    }

    /// Returns true only when a write fault consumed a currently lazy-free
    /// page.  The original VMA permissions are restored before retrying the
    /// instruction, making a post-MADV_FREE store observable to reclaim as a
    /// new generation rather than silently losing it.
    pub(super) fn consume_madvise_free_write_fault(
        &mut self,
        page: VirtAddr,
        _vma_flags: MappingFlags,
    ) -> bool {
        let Some(record) = self.madvise_free_pages.remove(&page) else {
            return false;
        };
        if let Ok((paddr, _flags, PageSize::Size4K)) = self.pt.query(page)
            && paddr == record.paddr
            && self
                .areas
                .find(page)
                .is_some_and(|area| area.backend().swap_reclaimable(paddr))
        {
            if self
                .pt
                .cursor()
                .remap(page, paddr, record.restore_flags)
                .is_ok()
            {
                drop(self.synchronize_tlb_after_mutation());
                return true;
            }
        }
        // Identity changed under a mapping transaction; leave the fault to
        // the normal backend COW/protection path rather than restoring stale
        // permissions.
        false
    }

    /// A successful mprotect which grants WRITE is itself a modification
    /// boundary: it must consume lazy-free generations because userspace can
    /// store through the newly writable PTE without taking a page fault.
    pub(crate) fn consume_madvise_free_write_range(&mut self, start: VirtAddr, size: usize) {
        let end = start + size;
        self.madvise_free_pages
            .retain(|&page, _| page < start || page >= end);
    }

    /// Drops resident private anonymous pages while keeping the VMA layout.
    pub fn discard_private_anonymous_pages(&mut self) {
        let ranges = self
            .areas
            .iter()
            .filter(|area| area.backend().is_private_anonymous())
            .map(|area| (area.start(), area.size()))
            .collect::<Vec<_>>();

        for (start, size) in ranges {
            if let Err(err) = self.discard_pages(start, size) {
                warn!("AddrSpace::discard_private_anonymous_pages: {start:?}+{size:#x}: {err:?}");
            }
        }
    }

    /// Reclaim the PTEs and backing frames the Linux OOM reaper may discard.
    ///
    /// The caller holds the address-space mmap-equivalent lock.  Do not race
    /// active user-I/O pins or userfaultfd registrations: this kernel has no
    /// nonblocking notifier protocol for either, so retaining the complete
    /// image and asking the caller to retry is the only safe outcome.
    /// Private file COW mappings are included, matching Linux's
    /// `vma_is_anonymous(vma) || !(vma->vm_flags & VM_SHARED)` rule.
    pub(crate) fn oom_reap_private_pages(&mut self) -> bool {
        if self.user_io_pins.progress().total() != 0 || self.uffd.is_some() {
            return false;
        }

        // Prefer MADV_FREE generations.  Their PTE identity and write-fault
        // cancellation are serialized by this mm lock, so a successful
        // discard leaves the normal anonymous missing-page path to zero-fill
        // on the next access. Pinned/busy/stale leaves are deliberately kept.
        let _ = self.reclaim_madvise_free_pages();

        let ranges = self
            .areas
            .iter()
            .filter(|area| area.backend().is_oom_reapable_private())
            .map(|area| (area.start(), area.size()))
            .collect::<Vec<_>>();

        for (start, size) in ranges {
            // discard_pages drains PTEs through the backend, waits for the
            // TLB generation grace period, and only then retires frames.
            if self.discard_pages(start, size).is_err() {
                return false;
            }
        }
        true
    }

    /// Reclaims only still-current MADV_FREE leaves.  The ledger is a
    /// capability, not an address hint: both physical identity and exclusive
    /// anonymous ownership must still match immediately before discard.
    pub(crate) fn reclaim_madvise_free_pages(&mut self) -> AxResult<usize> {
        let candidates: Vec<_> = self
            .madvise_free_pages
            .iter()
            .map(|(&page, &record)| (page, record))
            .collect();
        let mut reclaimed = 0usize;
        for (page, record) in candidates {
            let matches = self.pt.query(page).ok().is_some_and(|(paddr, _, size)| {
                size == PageSize::Size4K
                    && paddr == record.paddr
                    && self
                        .areas
                        .find(page)
                        .is_some_and(|area| area.backend().swap_reclaimable(paddr))
            });
            if !matches {
                self.madvise_free_pages.remove(&page);
                continue;
            }
            match self.discard_pages(page, PAGE_SIZE_4K) {
                Ok(()) => reclaimed = reclaimed.saturating_add(1),
                Err(error) if matches!(error.canonicalize(), AxError::ResourceBusy) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(reclaimed)
    }

    /// Removes mappings within the specified virtual address range.
    ///
    /// Returns an error if the address range is out of the address space or not
    /// aligned.
    pub(crate) fn unmap(&mut self, start: VirtAddr, size: usize) -> AxResult<DeferredUffdWake> {
        self.unmap_inner(start, size, true)
    }

    /// Removes a replacement destination while retaining its existing shared
    /// alias lease until the caller has either published the replacement VMA
    /// or explicitly pruned it.  This closes the old-lease-to-new-VMA gap of
    /// MAP_FIXED/SysV replacement transactions.
    pub(crate) fn unmap_preserving_shared_alias_bindings(
        &mut self,
        start: VirtAddr,
        size: usize,
    ) -> AxResult<DeferredUffdWake> {
        self.unmap_inner(start, size, false)
    }

    pub(super) fn unmap_inner(
        &mut self,
        start: VirtAddr,
        size: usize,
        prune_shared_alias_bindings: bool,
    ) -> AxResult<DeferredUffdWake> {
        self.validate_region(start, size)?;
        if size == 0 {
            return Ok(DeferredUffdWake::empty());
        }
        self.check_no_seal_overlap(start, size)?;
        self.check_no_user_io_pin_overlap(start, size, InvalidationReason::Unmap)?;
        // Clearing a middle slice of one DONTDUMP interval creates one extra
        // suffix record. Reserve it before any uprobe/PTE/VMA mutation.
        self.dontdump_ranges
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        // XOL is a kernel-owned special VMA.  Invalidate its registry token
        // under this destructive transaction lock before any PTE/VMA change.
        crate::uprobe::invalidate_xol_range_locked(self, start, size);
        self.ensure_4k_granularity(start, size)?;
        let next_generation = self.next_topology_generation()?;
        let mapping_mutations =
            prepare_unmap_mapping_mutations(&self.areas, &self.mapping_identities, start, size)?;
        let unmap_range = PageRange::new(start.as_usize(), size, PAGE_SIZE_4K).map_err(mm_error)?;
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
                })? {
                    OptionalUffdPlan::Noop => None,
                    plan @ OptionalUffdPlan::Armed(_) => Some(plan),
                }
            } else {
                None
            }
        };

        if let Err(error) = self.unmap_areas_with_tlb_grace(start, size) {
            if let Some(plan) = uffd_plan {
                self.uffd
                    .as_mut()
                    .expect("armed UFFD unmap plan lost its address-space state")
                    .abort_plan(plan);
            }
            return Err(error.into());
        }
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
        #[cfg(target_arch = "x86_64")]
        self.remove_cet_default_shadow_stack_extents_for_unmap(start, size);
        if prune_shared_alias_bindings {
            self.prune_shared_alias_bindings();
        }
        let wake = if let Some(plan) = uffd_plan {
            self.uffd
                .as_mut()
                .expect("armed UFFD unmap plan lost its address-space state")
                .commit_plan(plan)
        } else {
            DeferredUffdWake::empty()
        };
        commit_mapping_identity_mutations(&mut self.mapping_identities, &mapping_mutations);
        self.commit_topology_generation(next_generation);
        Ok(wake)
    }

    /// To process data in this area with the given function.
    ///
    /// Now it supports reading and writing data in the given interval.
    pub(super) fn process_area_data<F>(&self, start: VirtAddr, size: usize, mut f: F) -> AxResult
    where
        F: FnMut(VirtAddr, usize, usize),
    {
        if !self.contains_range(start, size) {
            ax_bail!(InvalidInput, "address out of range");
        }
        let mut cnt = 0;
        // If start is aligned to 4K, start_align_down will be equal to start_align_up.
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let end_align_up =
            VirtAddr::from(checked_align_up_4k(end.as_usize()).ok_or(AxError::InvalidInput)?);
        let pages =
            PageIter4K::new(start.align_down_4k(), end_align_up).ok_or(AxError::InvalidInput)?;
        for vaddr in pages {
            let (mut paddr, ..) = self.pt.query(vaddr).map_err(|_| AxError::BadAddress)?;

            let mut copy_size = (size - cnt).min(PAGE_SIZE_4K);

            if copy_size == 0 {
                break;
            }
            if vaddr == start.align_down_4k() && start.align_offset_4k() != 0 {
                let align_offset = start.align_offset_4k();
                copy_size = copy_size.min(PAGE_SIZE_4K - align_offset);
                paddr += align_offset;
            }
            f(phys_to_virt(paddr), cnt, copy_size);
            cnt += copy_size;
        }
        Ok(())
    }

    /// To read data from the address space.
    ///
    /// # Arguments
    ///
    /// * `start` - The start virtual address to read.
    /// * `buf` - The buffer to store the data.
    pub fn read(&self, start: VirtAddr, buf: &mut [u8]) -> AxResult {
        // SAFETY: `process_area_data` passes a direct-map pointer to `read_size` mapped bytes of
        // the current page, and `offset + read_size <= buf.len()`; kernel `buf` cannot alias user
        // frames.
        self.process_area_data(start, buf.len(), |src, offset, read_size| unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), buf.as_mut_ptr().add(offset), read_size);
        })
    }

    /// To write data to the address space.
    ///
    /// # Arguments
    ///
    /// * `start_vaddr` - The start virtual address to write.
    /// * `buf` - The buffer to write to the address space.
    pub fn write(&self, start: VirtAddr, buf: &[u8]) -> AxResult {
        let synchronize_instruction_stream = start.checked_add(buf.len()).is_some_and(|end| {
            self.areas.iter().any(|area| {
                area.start() < end
                    && start < area.end()
                    && area.flags().contains(MappingFlags::EXECUTE)
            })
        });
        // SAFETY: `process_area_data` passes a direct-map pointer to `write_size` mapped bytes of
        // the current page, and `offset + write_size <= buf.len()`; kernel `buf` cannot alias user
        // frames.
        let result = self.process_area_data(start, buf.len(), |dst, offset, write_size| unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr().add(offset), dst.as_mut_ptr(), write_size);
        });
        // Direct address-space writers include ptrace and process_vm_writev.
        // Synchronize even after partial failure because a prefix may already
        // have changed executable memory.
        if synchronize_instruction_stream {
            synchronize_executable_publication(MappingFlags::EXECUTE);
        }
        result
    }

    /// Replaces one byte in a private file-backed executable mapping without
    /// ever making its user PTE writable.  This is the uprobe overlay
    /// primitive: `populate_area(WRITE)` first takes the ordinary COW path,
    /// then the physical byte is changed while the leaf retains its RX VMA
    /// policy.  Shared mappings are rejected, so an INT3 cannot escape into
    /// another process or the inode page cache.
    pub(crate) fn uprobe_cow_patch_byte(&mut self, address: VirtAddr, byte: u8) -> AxResult<u8> {
        let page = address.align_down(PAGE_SIZE_4K);
        let (flags, private_file_cow) = {
            let area = self.areas.find(address).ok_or(AxError::BadAddress)?;
            (
                area.flags(),
                area.flags().contains(MappingFlags::EXECUTE)
                    && area.backend().is_private_cow()
                    && area
                        .backend()
                        .file_mapping()
                        .is_some_and(|mapping| mapping.sharing() == FileMappingSharing::Private),
            )
        };
        if !private_file_cow {
            return Err(AxError::PermissionDenied);
        }

        // COW's populate implementation treats WRITE as a private-copy
        // request even when the user VMA itself is RX.  It retains `flags`
        // for the final PTE, which is precisely the no-user-W/X overlay
        // invariant required by uprobes.
        self.populate_area(page, PAGE_SIZE_4K, MappingFlags::WRITE)?;
        let (_, leaf_flags, leaf_size) = self.pt.query(page).map_err(|_| AxError::BadAddress)?;
        if leaf_size != PageSize::Size4K
            || leaf_flags.contains(MappingFlags::WRITE)
            || !leaf_flags.contains(MappingFlags::EXECUTE)
        {
            return Err(AxError::BadState);
        }
        let mut previous = 0u8;
        // SAFETY: `process_area_data` passes a direct-map pointer to the one mapped byte at
        // `address`; volatile access keeps the patch a single byte store.
        self.process_area_data(address, 1, |dst, _, _| unsafe {
            previous = core::ptr::read_volatile(dst.as_ptr());
            core::ptr::write_volatile(dst.as_mut_ptr(), byte);
        })?;
        synchronize_executable_publication(flags);
        Ok(previous)
    }

    /// Restores an uprobe byte on the resident RX leaf which just generated a
    /// user #BP.  Unlike the general installer this path never populates,
    /// allocates, or sleeps: successful instruction fetch proves the leaf is
    /// resident, and the private file-COW validation prevents modifying inode
    /// cache or another mm.  It is used only while the uprobe registry lock
    /// serializes final-consumer retirement against a new registration.
    pub(crate) fn uprobe_restore_trapped_byte(
        &mut self,
        address: VirtAddr,
        byte: u8,
    ) -> AxResult<u8> {
        let page = address.align_down(PAGE_SIZE_4K);
        let flags = {
            let area = self.areas.find(address).ok_or(AxError::BadAddress)?;
            let private_file_cow = area.flags().contains(MappingFlags::EXECUTE)
                && area.backend().is_private_cow()
                && area
                    .backend()
                    .file_mapping()
                    .is_some_and(|mapping| mapping.sharing() == FileMappingSharing::Private);
            if !private_file_cow {
                return Err(AxError::PermissionDenied);
            }
            area.flags()
        };
        let (paddr, leaf_flags, leaf_size) =
            self.pt.query(page).map_err(|_| AxError::BadAddress)?;
        if leaf_size != PageSize::Size4K
            || leaf_flags.contains(MappingFlags::WRITE)
            || !leaf_flags.contains(MappingFlags::EXECUTE)
        {
            return Err(AxError::BadState);
        }
        let offset = address.as_usize() - page.as_usize();
        let target = axhal::mem::phys_to_virt(PhysAddr::from(paddr.as_usize() + offset));
        // SAFETY: `paddr` is the 4 KiB executable leaf translated above under the address-space
        // lock and `offset < 4096`, so `target` is a valid direct-map byte of that frame.
        let previous = unsafe {
            let pointer = target.as_mut_ptr();
            let previous = core::ptr::read_volatile(pointer);
            core::ptr::write_volatile(pointer, byte);
            previous
        };
        synchronize_executable_publication(flags);
        Ok(previous)
    }

    /// Copies from this address space for the task that currently owns it.
    ///
    /// Ordinary pages retain the direct-map fast path. Secret shared pages
    /// have no direct alias, so each VMA/page-sized piece is populated and
    /// copied through its backing's CPU-local secret window instead.
    pub(crate) fn current_uaccess_read(&mut self, start: VirtAddr, buf: &mut [u8]) -> AxResult {
        self.current_uaccess(start, buf, MappingFlags::READ)
    }

    /// See [`Self::current_uaccess_read`].
    pub(crate) fn current_uaccess_write(&mut self, start: VirtAddr, buf: &[u8]) -> AxResult {
        if buf.is_empty() {
            return Ok(());
        }
        if !self.contains_range(start, buf.len()) {
            return Err(AxError::BadAddress);
        }
        let end = start.checked_add(buf.len()).ok_or(AxError::BadAddress)?;
        let mut cursor = start;
        let mut copied = 0;
        while cursor < end {
            let (area_end, area_flags, backend) = {
                let area = self.areas.find(cursor).ok_or(AxError::BadAddress)?;
                if area.start() > cursor || !area.flags().contains(MappingFlags::WRITE) {
                    return Err(AxError::PermissionDenied);
                }
                (area.end(), area.flags(), area.backend().clone())
            };
            let page_end = cursor + PAGE_SIZE_4K - cursor.align_offset_4k();
            let piece_end = area_end.min(end).min(page_end);
            self.populate_area(cursor.align_down_4k(), PAGE_SIZE_4K, MappingFlags::WRITE)?;
            let piece_len = piece_end - cursor;
            let piece = &buf[copied..copied + piece_len];
            match backend {
                Backend::Shared(shared) if shared.is_secret() => {
                    let offset = shared
                        .backing_offset(cursor.as_usize())
                        .ok_or(AxError::BadAddress)?;
                    shared.pages().write_bytes(offset, piece)?;
                }
                _ => self.write(cursor, piece)?,
            }
            if area_flags.contains(MappingFlags::EXECUTE) {
                synchronize_executable_publication(MappingFlags::EXECUTE);
            }
            cursor = piece_end;
            copied += piece_len;
        }
        Ok(())
    }

    pub(super) fn current_uaccess(
        &mut self,
        start: VirtAddr,
        buf: &mut [u8],
        access_flags: MappingFlags,
    ) -> AxResult {
        if buf.is_empty() {
            return Ok(());
        }
        if !self.contains_range(start, buf.len()) {
            return Err(AxError::BadAddress);
        }
        let end = start.checked_add(buf.len()).ok_or(AxError::BadAddress)?;
        let mut cursor = start;
        let mut copied = 0;
        while cursor < end {
            let (area_end, backend) = {
                let area = self.areas.find(cursor).ok_or(AxError::BadAddress)?;
                if area.start() > cursor || !area.flags().contains(access_flags) {
                    return Err(AxError::PermissionDenied);
                }
                (area.end(), area.backend().clone())
            };
            let piece_end = area_end
                .min(end)
                .min(cursor + PAGE_SIZE_4K - cursor.align_offset_4k());
            let page_start = cursor.align_down_4k();
            self.populate_area(page_start, PAGE_SIZE_4K, access_flags)?;
            let piece_len = piece_end - cursor;
            let piece = &mut buf[copied..copied + piece_len];
            match backend {
                Backend::Shared(shared) if shared.is_secret() => {
                    let offset = shared
                        .backing_offset(cursor.as_usize())
                        .ok_or(AxError::BadAddress)?;
                    shared.pages().read_bytes(offset, piece)?;
                }
                _ => self.read(cursor, piece)?,
            }
            cursor = piece_end;
            copied += piece_len;
        }
        Ok(())
    }

    /// Returns whether a range overlaps a secret-memory VMA.  Such frames
    /// must never be accessed through the generic direct-map copy helpers.
    pub(crate) fn has_secret_mapping(&self, start: VirtAddr, len: usize) -> bool {
        len != 0
            && start.checked_add(len).is_some_and(|end| {
                self.areas.iter().any(|area| {
                    area.start() < end && start < area.end() && area.backend().is_secret()
                })
            })
    }

    /// Updates mapping within the specified virtual address range.
    ///
    /// Returns an error if the address range is out of the address space or not
    /// aligned.
    pub(crate) fn prepare_protect(
        &mut self,
        start: VirtAddr,
        size: usize,
        flags: MappingFlags,
    ) -> AxResult<PreparedProtect<'_>> {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        self.prepare_protect_ranges(
            start,
            size,
            vec![PreparedProtectRange { start, end, flags }],
        )
    }

    /// Prepares one atomic pkey_mprotect transaction.  READ_IMPLIES_EXEC is
    /// evaluated against each source VMA because an executable personality is
    /// suppressed for file mappings on noexec mounts.
    pub(crate) fn prepare_pkey_protect(
        &mut self,
        start: VirtAddr,
        size: usize,
        requested: MappingFlags,
        key: u8,
        read_implies_exec: bool,
    ) -> AxResult<PreparedProtect<'_>> {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let mut ranges = Vec::new();
        let mut cursor = start;
        while cursor < end {
            let Some(area) = self.areas.find(cursor) else {
                return Err(AxError::NoMemory);
            };
            if area.start() > cursor {
                return Err(AxError::NoMemory);
            }
            ranges.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            let may_execute = area
                .backend()
                .file_mapping()
                .is_none_or(|mapping| mapping.may_protect().contains(MappingFlags::EXECUTE));
            let mut flags = requested;
            if area.flags().contains(MappingFlags::SHADOW_STACK) {
                // pkey_mprotect may rekey a shadow stack, but never turns it
                // into a conventional writable/executable mapping.
                if requested != MappingFlags::READ
                    && requested != (MappingFlags::READ | MappingFlags::WRITE)
                    && !requested.is_empty()
                {
                    return Err(AxError::InvalidInput);
                }
                flags = (flags - MappingFlags::EXECUTE) | MappingFlags::SHADOW_STACK;
            }
            if read_implies_exec && may_execute && flags.contains(MappingFlags::READ) {
                flags |= MappingFlags::EXECUTE;
            }
            if area.flags().contains(MappingFlags::SHADOW_STACK) {
                flags -= MappingFlags::EXECUTE;
            }
            let segment_end = area.end().min(end);
            ranges.push(PreparedProtectRange {
                start: cursor,
                end: segment_end,
                flags: flags.with_pkey(key),
            });
            cursor = segment_end;
        }
        self.prepare_protect_ranges(start, size, ranges)
    }
}
