//! AddrSpace: construction, geometry, mapping identities and guard intervals.

use super::*;

impl AddrSpace {
    pub(super) const STACK_GUARD_GAP_PAGES: usize = 256;

    /// Publishes an alias-creation fence before a cache eviction changes any
    /// PTE permission.  All allocation is done first so reservation abort is
    /// infallible once write protection has started.
    pub(crate) fn publish_file_eviction_fence(
        &mut self,
        cache: axfs::CachedFileIdentity,
        page_number: u32,
    ) -> AxResult<FileEvictionFenceKey> {
        if self.file_eviction_fences.len() == self.file_eviction_fences.capacity() {
            self.file_eviction_fences
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
        }
        let generation = self
            .next_file_eviction_fence_generation
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        self.next_file_eviction_fence_generation = generation;
        let key = FileEvictionFenceKey {
            cache,
            page_number,
            generation,
        };
        self.file_eviction_fences.push(key);
        Ok(key)
    }

    /// True when publishing or upgrading an alias for this cache page must
    /// retry after the in-flight eviction completes.  Permission revocation
    /// and unmap intentionally do not consult this table.
    pub(crate) fn file_eviction_fenced(
        &self,
        cache: axfs::CachedFileIdentity,
        page_number: u32,
    ) -> bool {
        self.file_eviction_fences
            .iter()
            .any(|key| key.cache == cache && key.page_number == page_number)
    }

    /// Finds the first cache page fenced inside a prospective alias-creating
    /// range.  Readers and destructive/revoking paths deliberately do not
    /// call this: only operations that can create, copy, or upgrade a file
    /// alias are required to wait.
    pub(crate) fn file_eviction_retry_for_range(
        &self,
        start: VirtAddr,
        size: usize,
    ) -> Option<FileEvictionRetry> {
        let end = start.checked_add(size)?;
        for key in &self.file_eviction_fences {
            for area in self.areas.iter() {
                let Backend::File(file) = area.backend() else {
                    continue;
                };
                if let Some(cache) = file.cache_for_eviction_alias(key.cache, key.page_number)
                    && let Some(alias) = file.eviction_alias_address(key.page_number)
                    && alias >= start
                    && alias < end
                    && area.start() <= alias
                    && alias < area.end()
                {
                    return Some(FileEvictionRetry::new(cache, *key));
                }
            }
        }
        None
    }

    /// Completes one prepare transaction and makes waiting mutations eligible
    /// to revalidate.  A missing key is an invariant violation: only the
    /// reservation which published this generation may retire it.
    pub(crate) fn complete_file_eviction_fence(&mut self, key: FileEvictionFenceKey) {
        let index = self
            .file_eviction_fences
            .iter()
            .position(|active| *active == key)
            .expect("file eviction reservation lost its fence");
        self.file_eviction_fences.swap_remove(index);
    }

    /// Returns the address space base.
    pub const fn base(&self) -> VirtAddr {
        self.va_range.start
    }

    /// Returns the address space end.
    pub const fn end(&self) -> VirtAddr {
        self.va_range.end
    }

    /// Assigns an x86 protection key to resident PTE leaves in a fully
    /// validated VMA range. Missing pages deliberately need no PTE update;
    /// their key is installed by the mapping metadata on first population.
    pub(crate) fn set_pkey(&mut self, start: VirtAddr, size: usize, key: u8) -> AxResult {
        let pkey = Pkey::new(key).ok_or(AxError::InvalidInput)?;
        self.validate_region(start, size)?;
        let leaves = self.pt.collect_mapped_leaves(start, size)?;
        // The key is VMA state, not merely a currently-resident PTE bit.
        // MemorySet's protected-range transaction splits boundary VMAs before
        // publishing the replacement flags, so later demand faults, COW and
        // fork cloning receive the same key through `MappingFlags`.
        self.areas
            .protect_with_limit(
                start,
                size,
                |_, flags| Some(flags.with_pkey(key)),
                &mut self.pt,
                MAX_VMA_FRAGMENTS,
            )
            .map_err(AxError::from)?;
        let mut cursor = self.pt.cursor();
        for (vaddr, ..) in leaves {
            cursor
                .set_pkey(vaddr, pkey)
                .expect("preflighted pkey leaf must remain mapped");
        }
        drop(cursor);
        let _ = self.tlb.synchronize_after_mutation();
        Ok(())
    }

    /// Validates that changing a key cannot require an unsupported huge-leaf
    /// demotion.  It allocates every resident-leaf record before any VMA or
    /// PTE mutation, allowing callers to reject a pkey_mprotect request
    /// before beginning its ordinary protection transaction.
    pub(crate) fn preflight_set_pkey(
        &self,
        start: VirtAddr,
        size: usize,
    ) -> AxResult<Vec<(VirtAddr, PhysAddr, MappingFlags, PageSize)>> {
        self.validate_region(start, size)?;
        // Validate every resident leaf before publishing VMA metadata. A
        // partial huge leaf is handled by `prepare_pkey_demotion`.
        let leaves = self.pt.collect_overlapping_mapped_leaves(start, size)?;
        Ok(leaves)
    }

    /// Reserves lower-level page tables for huge leaves touched partially by
    /// a pkey range. Fully covered huge leaves retain their original size.
    pub(crate) fn prepare_pkey_demotion(
        &self,
        start: VirtAddr,
        size: usize,
    ) -> AxResult<PreparedPkeyDemotion> {
        self.validate_region(start, size)?;
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let present = self.pt.collect_overlapping_mapped_leaves(start, size)?;
        let mut leaves = Vec::new();
        leaves
            .try_reserve(present.len())
            .map_err(|_| AxError::NoMemory)?;
        for (vaddr, paddr, _, page_size) in present {
            if page_size == PageSize::Size4K {
                continue;
            }
            let leaf_end = vaddr
                .checked_add(page_size as usize)
                .ok_or(AxError::InvalidInput)?;
            if vaddr >= start && leaf_end <= end {
                continue;
            }
            let frames = match page_size {
                PageSize::Size2M => 1,
                PageSize::Size1G => {
                    let chunk = PageSize::Size2M as usize;
                    let begin = usize::from(vaddr.max(start));
                    let finish = usize::from(leaf_end.min(end));
                    let first = begin / chunk;
                    let last = (finish - 1) / chunk;
                    1 + (first..=last)
                        .filter(|&index| begin > index * chunk || finish < (index + 1) * chunk)
                        .count()
                }
                PageSize::Size4K | PageSize::Size1M => return Err(AxError::InvalidInput),
            };
            leaves.push(PreparedPkeyLeaf {
                vaddr,
                paddr,
                size: page_size,
                cow_backing: matches!(
                    self.areas.find(vaddr).map(|area| area.backend()),
                    Some(Backend::Cow(_))
                ),
                tables: PreparedPageTableFrames::try_new(frames)
                    .map_err(PreparedPkeyDemotion::prepare_table_error)?,
            });
        }
        Ok(PreparedPkeyDemotion { leaves, start, end })
    }

    /// Returns the stable policy identity of this address space.
    pub(crate) const fn address_space_id(&self) -> AddressSpaceId {
        self.address_space_id
    }

    /// Returns the address space size.
    pub fn size(&self) -> usize {
        self.va_range.size()
    }

    /// Returns the reference to the inner page table.
    pub const fn page_table(&self) -> &PageTable {
        &self.pt
    }

    /// Returns a mutable reference to the inner page table.
    pub const fn page_table_mut(&mut self) -> &mut PageTable {
        &mut self.pt
    }

    /// Completes a direct leaf-PTE mutation that happened outside a backend
    /// operation.  The caller holds this address space's write lock.
    pub(crate) fn synchronize_pte_mutation(&self) {
        let _ = self.tlb.synchronize_after_mutation();
    }

    /// Returns the root physical address of the inner page table.
    pub const fn page_table_root(&self) -> PhysAddr {
        self.pt.root_paddr()
    }

    /// Returns the page-table root and bounded hardware-ASID identity.
    pub const fn address_space_token(&self) -> AddressSpaceToken {
        AddressSpaceToken::new(self.pt.root_paddr(), self.hardware_asid)
    }

    /// Publishes this address space as active on the current CPU at a
    /// scheduler context-switch boundary.
    ///
    /// Residency is published before sampling the generation. The bit is a
    /// conservative, monotonic upper bound because task-extension hooks run
    /// before the hardware page-table switch; clearing it there could let a
    /// writer release mappings while the old CR3 is still live. If a page
    /// table writer snapshots the CPU before this store, the generation load
    /// observes the writer's subsequent publication and performs the local
    /// repair itself; if it snapshots after the store, the CPU is included in
    /// the targeted shootdown.
    pub(crate) fn tlb_state(&self) -> Arc<TlbState> {
        self.tlb.clone()
    }

    pub(crate) fn ldt_snapshot(&self) -> Option<Arc<Ldt>> {
        self.tlb.snapshot_ldt()
    }

    pub(crate) fn replace_ldt_entry(&mut self, info: UserDesc, oldmode: bool) -> AxResult {
        let index = info.entry_number as usize;
        if index >= ENTRIES {
            return Err(AxError::InvalidInput);
        }
        let old = self.tlb.snapshot_ldt();
        let mut next = Ldt::new(core::cmp::max(
            index + 1,
            old.as_ref().map_or(0, |table| table.len()),
        ))?;
        if let Some(old) = old.as_ref() {
            old.copy_into(&mut next);
        }
        next.set(index, Ldt::descriptor(info, oldmode)?);
        let retired = self
            .tlb
            .replace_ldt(Some(Arc::try_new(next).map_err(|_| AxError::NoMemory)?));

        // Publish the new descriptor locally before remote acknowledgements
        // make the old backing allocation reclaimable.
        {
            let _guard = NoPreemptIrqSave::new();
            self.tlb.reload_current_ldt();
        }
        let grace = self.synchronize_tlb_after_mutation();
        drop(grace);
        drop(retired);
        Ok(())
    }

    /// Completes one PTE mutation's full local flush and targeted grace.
    ///
    /// The helper advances the generation before taking the active snapshot;
    /// callers must invoke it after publishing page-table stores but before
    /// releasing any retired mapping/frame ownership.
    pub(crate) fn synchronize_tlb_after_mutation(&self) -> impl Drop {
        self.tlb.synchronize_after_mutation()
    }

    /// Checks if the address space contains the given address range.
    pub fn contains_range(&self, start: VirtAddr, size: usize) -> bool {
        self.va_range.contains(start) && (self.va_range.end - start) >= size
    }

    /// Creates a new empty address space.
    pub fn new_empty(base: VirtAddr, size: usize) -> AxResult<Self> {
        #[cfg(test)]
        crate::test_support::ensure_host_memory();

        let va_range = VirtAddrRange::try_from_start_size(base, size).ok_or(AxError::NoMemory)?;
        let (address_space_id, topology_mapping_id, topology_generation, user_io_pins) =
            new_user_io_policy()?;
        let hardware_asid = reserve_hardware_address_space_id();
        let mut active_long_term_cow_pins = Vec::new();
        active_long_term_cow_pins
            .try_reserve_exact(USER_IO_PIN_MAX_TOKENS as usize)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            va_range,
            address_space_id,
            hardware_asid,
            maxrss_kb: AtomicU64::new(0),
            oom_reap_state: AtomicU8::new(0),
            thp_disable_mode: ThpDisableMode::Enabled,
            tlb: Arc::try_new(TlbState::new()).map_err(|_| AxError::NoMemory)?,
            topology_mapping_id,
            topology_generation,
            areas: MemorySet::new(),
            mapping_identities: MappingIdentityIndex::new(),
            growdown_starts: BTreeSet::new(),
            madvise_guard_ranges: BTreeMap::new(),
            madvise_hwpoison_ranges: BTreeMap::new(),
            madvise_free_pages: BTreeMap::new(),
            next_madvise_free_generation: 0,
            wipe_on_fork_ranges: BTreeMap::new(),
            dontfork_ranges: BTreeMap::new(),
            dontdump_ranges: Vec::new(),
            locked_ranges: BTreeMap::new(),
            file_eviction_fences: Vec::new(),
            next_file_eviction_fence_generation: 0,
            cet_default_shadow_stacks: Vec::new(),
            alias_bindings: BTreeMap::new(),
            swapped: BTreeMap::new(),
            user_io_pins,
            active_long_term_cow_pins,
            uffd: None,
            lock_future_mappings: false,
            lock_future_on_fault: false,
            pt: PageTable::try_new().map_err(|_| AxError::NoMemory)?,
        })
    }

    /// The mmap-lock equivalent used by operations that Linux specifies as
    /// killable.  The sleeping mutex registers an event listener before its
    /// SeqCst owner recheck, so unlock notification cannot be lost; only a
    /// pending SIGKILL cancels the wait, matching fatal_signal_pending().
    pub(crate) fn lock_interruptibly(
        handle: &Arc<Mutex<Self>>,
    ) -> AxResult<axsync::MutexGuard<'_, Self>> {
        axsync::lock_interruptible(handle, || {
            let current = axtask::current();
            let thread = current.as_thread();
            has_pending_sigkill(thread)
                || thread.proc_data.should_exit_for_exec(thread.kernel_tid())
        })
        .ok_or(AxError::Interrupted)
    }

    pub(super) fn prepare_fresh_mapping_lineage(&mut self) -> AxResult<MappingLineage> {
        reserve_mapping_identity_slot(&mut self.mapping_identities, MAX_MAPPING_LINEAGES)?;
        let (lineage, identity) = allocate_mapping_identity()?;
        debug_assert_eq!(lineage.get(), identity.id.get());
        debug_assert_ne!(identity.id, self.topology_mapping_id);
        self.mapping_identities.insert_reserved(lineage, identity)?;
        Ok(lineage)
    }

    pub(super) fn remove_mapping_lineage_if_unused(&mut self, lineage: MappingLineage) -> bool {
        if self.areas.iter().any(|area| area.lineage() == lineage) {
            return false;
        }
        self.mapping_identities.remove(lineage).is_some()
    }

    /// Synchronizes this mm's reverse-map leases after a shared mapping
    /// topology change.  The caller supplies the owning Arc at syscall/fork
    /// boundaries; ordinary `map` remains a pure address-space operation.
    pub(crate) fn sync_shared_alias_bindings(
        &mut self,
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult {
        let mut keys = Vec::new();
        keys.try_reserve(self.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in self.areas.iter() {
            if let Some(key) = area.backend().shared_backing_key() {
                keys.push(key);
            }
        }
        keys.sort_unstable();
        keys.dedup();

        self.bind_shared_alias_keys(&keys, aspace)?;
        self.alias_bindings
            .retain(|key, _| keys.binary_search(key).is_ok());
        Ok(())
    }

    pub(super) fn bind_shared_alias_keys(
        &mut self,
        keys: &[SharedBackingKey],
        aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult {
        let mut inserted = Vec::new();
        inserted
            .try_reserve(keys.len())
            .map_err(|_| AxError::NoMemory)?;
        for key in keys.iter().copied() {
            if self.alias_bindings.contains_key(&key) {
                continue;
            }
            match AliasLease::try_new(key, aspace, self.address_space_id) {
                Ok(lease) => {
                    self.alias_bindings.insert(key, lease);
                    inserted.push(key);
                }
                Err(error) => {
                    for key in inserted {
                        self.alias_bindings.remove(&key);
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn commit_shared_alias_binding(&mut self, pending: PendingAliasLease) {
        let Some(lease) = pending.commit() else {
            return;
        };
        let key = lease.key();
        // Registry admission is per-(mm, backing).  A concurrent mapper may
        // have committed the same binding between lock-external preparation
        // and this publication; retain its lease and discard this no-longer
        // needed duplicate ownership instead of overwriting it.
        if self.alias_bindings.contains_key(&key) {
            drop(lease);
        } else {
            self.alias_bindings.insert(key, lease);
        }
    }

    pub(super) fn prune_shared_alias_bindings(&mut self) {
        self.alias_bindings.retain(|key, _| {
            self.areas
                .iter()
                .any(|area| area.backend().shared_backing_key() == Some(*key))
        });
    }

    /// Completes a caller-owned replacement interval that temporarily kept
    /// its old reverse-map lease alive across destructive unmap.
    pub(crate) fn finish_shared_alias_binding_transition(&mut self) {
        self.prune_shared_alias_bindings();
    }

    pub(super) fn mapping_identity(
        &self,
        lineage: MappingLineage,
    ) -> AxResult<MappingIdentityState> {
        mapping_identity(&self.mapping_identities, lineage)
    }

    pub(super) fn commit_mapping_generation(
        &mut self,
        lineage: MappingLineage,
        generation: MappingGeneration,
    ) {
        self.mapping_identities
            .get_mut(lineage)
            .expect("existing mapping lineage disappeared")
            .generation = generation;
    }

    pub(super) fn refresh_growdown_starts(&mut self) {
        PreparedProtect::refresh_growdown_starts(&self.areas, &mut self.growdown_starts);
    }

    pub fn mark_growdown(&mut self, start: VirtAddr) {
        self.growdown_starts.insert(start);
        self.refresh_growdown_starts();
    }

    /// Installs Linux MADV_GUARD_INSTALL state on an already mapped private
    /// anonymous interval.  Guard pages retain their VMA identity but are
    /// deliberately non-resident and reject every subsequent access.
    pub fn install_madvise_guard(&mut self, start: VirtAddr, size: usize) -> AxResult<()> {
        self.validate_region(start, size)?;
        if self.range_is_locked(start, size) {
            return Err(AxError::ResourceBusy);
        }
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let mut cursor = start;
        while cursor < end {
            let area = self.areas.find(cursor).ok_or(AxError::NoMemory)?;
            if area.start() > cursor || !area.backend().is_private_anonymous() {
                return Err(AxError::InvalidInput);
            }
            cursor = area.end().min(end);
        }
        self.discard_pages(start, size)?;
        Self::clear_interval(&mut self.madvise_guard_ranges, start, size);
        self.madvise_guard_ranges.insert(start, end);
        Ok(())
    }

    /// Removes a previously installed MADV guard.  The next access follows
    /// the ordinary anonymous missing-page path and receives a fresh zeroed
    /// page; removed guards never resurrect discarded data.
    pub fn remove_madvise_guard(&mut self, start: VirtAddr, size: usize) -> AxResult<()> {
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let mut cursor = start;
        while cursor < end {
            let Some((&guard_start, &guard_end)) =
                self.madvise_guard_ranges.range(..=cursor).next_back()
            else {
                return Err(AxError::InvalidInput);
            };
            if guard_start > cursor || guard_end <= cursor {
                return Err(AxError::InvalidInput);
            }
            cursor = guard_end.min(end);
        }
        // A remove request may span adjacent guard records. Split only at
        // record boundaries; preserving prefix/suffix guards is required for
        // partial removal.
        let overlapping: Vec<_> = self
            .madvise_guard_ranges
            .range(..end)
            .filter_map(|(&guard_start, &guard_end)| {
                (guard_end > start).then_some((guard_start, guard_end))
            })
            .collect();
        for (guard_start, guard_end) in overlapping {
            self.madvise_guard_ranges.remove(&guard_start);
            if guard_start < start {
                self.madvise_guard_ranges.insert(guard_start, start);
            }
            if guard_end > end {
                self.madvise_guard_ranges.insert(end, guard_end);
            }
        }
        Ok(())
    }

    pub(super) fn is_madvise_guard(&self, address: VirtAddr) -> bool {
        self.madvise_guard_ranges
            .range(..=address)
            .next_back()
            .is_some_and(|(_, &end)| address < end)
    }

    pub fn install_madvise_hwpoison(&mut self, start: VirtAddr, size: usize) -> AxResult<()> {
        self.validate_region(start, size)?;
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        self.discard_pages(start, size)?;
        Self::clear_interval(&mut self.madvise_hwpoison_ranges, start, size);
        self.madvise_hwpoison_ranges.insert(start, end);
        Ok(())
    }

    pub(super) fn is_madvise_hwpoison(&self, address: VirtAddr) -> bool {
        self.madvise_hwpoison_ranges
            .range(..=address)
            .next_back()
            .is_some_and(|(_, &end)| address < end)
    }

    /// Relocates advisory fault sidecars after a committed mremap.  These
    /// ranges are part of a VMA's user-visible fault contract, so leaving
    /// them at the old virtual address would poison a later unrelated mmap.
    pub(crate) fn prepare_remap_madvise_sidecars(
        &self,
        source: VirtAddr,
        old_size: usize,
        destination: VirtAddr,
        new_size: usize,
        dont_unmap: bool,
    ) -> PreparedMadviseSidecarRemap {
        fn relocate(
            map: &mut BTreeMap<VirtAddr, VirtAddr>,
            source: VirtAddr,
            old_size: usize,
            destination: VirtAddr,
            new_size: usize,
            dont_unmap: bool,
        ) {
            let Some(source_end) = source.checked_add(old_size) else {
                return;
            };
            let Some(destination_end) = destination.checked_add(new_size) else {
                return;
            };
            let source_records: Vec<_> = map
                .range(..source_end)
                .filter_map(|(&start, &end)| (end > source).then_some((start, end)))
                .collect();
            let mut moved = Vec::new();
            for (start, end) in source_records {
                let clip_start = start.max(source);
                let clip_end = end.min(source_end);
                if clip_start >= clip_end {
                    continue;
                }
                let offset = clip_start.sub_addr(source);
                let Some(new_start) = destination.checked_add(offset) else {
                    continue;
                };
                let new_end = new_start
                    .checked_add(clip_end.sub_addr(clip_start))
                    .map(|end| end.min(destination_end));
                if let Some(new_end) = new_end.filter(|end| *end > new_start) {
                    moved.push((new_start, new_end));
                }
            }
            if !dont_unmap {
                AddrSpace::clear_interval(map, source, old_size);
            }
            AddrSpace::clear_interval(map, destination, new_size);
            for (start, end) in moved {
                map.insert(start, end);
            }
        }
        let mut guard_ranges = self.madvise_guard_ranges.clone();
        let mut hwpoison_ranges = self.madvise_hwpoison_ranges.clone();
        let mut free_pages = self.madvise_free_pages.clone();
        relocate(
            &mut guard_ranges,
            source,
            old_size,
            destination,
            new_size,
            dont_unmap,
        );
        relocate(
            &mut hwpoison_ranges,
            source,
            old_size,
            destination,
            new_size,
            dont_unmap,
        );
        let source_end = source.checked_add(old_size);
        let destination_end = destination.checked_add(new_size);
        if let (Some(source_end), Some(destination_end)) = (source_end, destination_end) {
            let moved: Vec<_> = free_pages
                .range(source..source_end)
                .map(|(&page, &generation)| (page, generation))
                .collect();
            if !dont_unmap {
                free_pages.retain(|&page, _| page < source || page >= source_end);
            }
            free_pages.retain(|&page, _| page < destination || page >= destination_end);
            for (page, generation) in moved {
                if let Some(new_page) = destination.checked_add(page.sub_addr(source))
                    && new_page < destination_end
                {
                    free_pages.insert(new_page, generation);
                }
            }
        }
        PreparedMadviseSidecarRemap {
            guard_ranges,
            hwpoison_ranges,
            free_pages,
        }
    }

    pub(crate) fn commit_prepared_madvise_sidecars(
        &mut self,
        prepared: PreparedMadviseSidecarRemap,
    ) {
        self.madvise_guard_ranges = prepared.guard_ranges;
        self.madvise_hwpoison_ranges = prepared.hwpoison_ranges;
        self.madvise_free_pages = prepared.free_pages;
    }

    /// Returns the current VMA start for a MAP_GROWSDOWN mapping containing
    /// `address`.  The grow-down identity is deliberately kept out of the
    /// generic VMA flags: it is fault policy, not a hardware permission bit.
    /// `mprotect(PROT_GROWSDOWN)` is the one non-fault consumer of that
    /// identity and must extend its operation to this moving VMA boundary.
    pub(crate) fn growdown_start_containing(&self, address: VirtAddr) -> Option<VirtAddr> {
        let area = self.find_area(address)?;
        self.growdown_starts
            .contains(&area.start())
            .then_some(area.start())
    }

    pub(super) fn move_growdown_start(&mut self, old_start: VirtAddr, new_start: VirtAddr) {
        if self.growdown_starts.remove(&old_start) {
            self.growdown_starts.insert(new_start);
        }
    }

    pub(super) fn insert_interval(
        ranges: &mut BTreeMap<VirtAddr, VirtAddr>,
        start: VirtAddr,
        end: VirtAddr,
    ) {
        if start >= end {
            return;
        }

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

    pub(super) fn clear_interval(
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

    pub(super) fn interval_end_covering(
        ranges: &BTreeMap<VirtAddr, VirtAddr>,
        addr: VirtAddr,
    ) -> Option<VirtAddr> {
        ranges
            .range(..=addr)
            .last()
            .and_then(|(&range_start, &range_end)| {
                (range_start <= addr && range_end > addr).then_some(range_end)
            })
    }

    pub(super) fn next_interval_start(
        ranges: &BTreeMap<VirtAddr, VirtAddr>,
        addr: VirtAddr,
        limit: VirtAddr,
    ) -> Option<VirtAddr> {
        ranges
            .range(addr..)
            .filter_map(|(&range_start, _)| {
                (range_start > addr && range_start < limit).then_some(range_start)
            })
            .next()
    }

    pub(super) fn interval_overlaps(
        ranges: &BTreeMap<VirtAddr, VirtAddr>,
        start: VirtAddr,
        end: VirtAddr,
    ) -> bool {
        ranges
            .range(..end)
            .any(|(&range_start, &range_end)| range_end > start && range_start < end)
    }

    pub(super) fn try_clone_interval_vec(
        ranges: &[(VirtAddr, VirtAddr)],
        extra_capacity: usize,
    ) -> AxResult<Vec<(VirtAddr, VirtAddr)>> {
        let capacity = ranges
            .len()
            .checked_add(extra_capacity)
            .ok_or(AxError::NoMemory)?;
        let mut cloned = Vec::new();
        cloned
            .try_reserve_exact(capacity)
            .map_err(|_| AxError::NoMemory)?;
        cloned.extend_from_slice(ranges);
        Ok(cloned)
    }

    /// Removes one interval from a pre-reserved sorted vector.  Splitting one
    /// containing range is the only operation that can grow the vector.
    pub(super) fn clear_interval_vec(
        ranges: &mut Vec<(VirtAddr, VirtAddr)>,
        start: VirtAddr,
        size: usize,
    ) {
        if size == 0 {
            return;
        }
        let end = start + size;
        let mut index = 0usize;
        while index < ranges.len() {
            let (range_start, range_end) = ranges[index];
            if range_end <= start {
                index += 1;
                continue;
            }
            if range_start >= end {
                break;
            }
            if range_start < start && range_end > end {
                ranges[index].1 = start;
                debug_assert!(ranges.len() < ranges.capacity());
                ranges.insert(index + 1, (end, range_end));
                break;
            }
            if range_start < start {
                ranges[index].1 = start;
                index += 1;
                continue;
            }
            if range_end > end {
                ranges[index] = (end, range_end);
                break;
            }
            ranges.remove(index);
        }
    }

    /// Adds one range to a pre-reserved sorted, coalesced interval vector.
    pub(super) fn insert_interval_vec(
        ranges: &mut Vec<(VirtAddr, VirtAddr)>,
        start: VirtAddr,
        end: VirtAddr,
    ) {
        if start >= end {
            return;
        }
        let mut new_start = start;
        let mut new_end = end;
        let index = ranges.partition_point(|(_, range_end)| *range_end < start);
        while index < ranges.len() && ranges[index].0 <= new_end {
            let (range_start, range_end) = ranges.remove(index);
            new_start = new_start.min(range_start);
            new_end = new_end.max(range_end);
        }
        debug_assert!(ranges.len() < ranges.capacity());
        ranges.insert(index, (new_start, new_end));
    }

    pub(super) fn interval_vec_end_covering(
        ranges: &[(VirtAddr, VirtAddr)],
        addr: VirtAddr,
    ) -> Option<VirtAddr> {
        let index = ranges.partition_point(|(start, _)| *start <= addr);
        index
            .checked_sub(1)
            .and_then(|index| (ranges[index].1 > addr).then_some(ranges[index].1))
    }

    pub(super) fn next_interval_vec_start(
        ranges: &[(VirtAddr, VirtAddr)],
        addr: VirtAddr,
        limit: VirtAddr,
    ) -> Option<VirtAddr> {
        let index = ranges.partition_point(|(start, _)| *start <= addr);
        ranges
            .get(index)
            .and_then(|(start, _)| (*start < limit).then_some(*start))
    }
}
