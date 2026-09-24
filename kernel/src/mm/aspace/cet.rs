//! AddrSpace: CET shadow stacks and remaining area accessors.

use super::*;

impl AddrSpace {
    #[cfg(target_arch = "x86_64")]
    /// Validates one registered default CET stack as a logical extent. A
    /// `pkey_mprotect(PROT_READ)` may split its VMA at either boundary while
    /// retaining SHSTK on every fragment; that must not invalidate the SSP
    /// lease merely because no single VMA still has the original size.
    pub(crate) fn cet_shadow_stack_extent_covers(&self, start: VirtAddr, size: usize) -> bool {
        let Some(end) = start.checked_add(size) else {
            return false;
        };
        if size == 0 {
            return false;
        }
        let mut cursor = start;
        while cursor < end {
            let Some(area) = self.find_area(cursor) else {
                return false;
            };
            let flags = area.flags();
            if area.start() > cursor
                || !flags
                    .contains(MappingFlags::USER | MappingFlags::READ | MappingFlags::SHADOW_STACK)
                // WRITE here is the VMA's shadow-write policy, not ordinary
                // PTE writability.  A live default SHSTK VMA intentionally
                // retains it so its leaves encode W=0,D=1.
                || flags.contains(MappingFlags::EXECUTE)
            {
                return false;
            }
            let next = area.end().min(end);
            if next <= cursor {
                return false;
            }
            cursor = next;
        }
        true
    }

    /// Registers the task-owned default stack only after its VMA is live.
    /// The caller retains responsibility for undoing the just-created VMA if
    /// this fallible publication cannot reserve its registry slot.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn register_cet_default_shadow_stack(
        &mut self,
        task_id: u32,
        start: VirtAddr,
        size: usize,
    ) -> AxResult {
        self.register_cet_default_shadow_stack_with_ownership(
            task_id,
            start,
            size,
            CetDefaultShadowStackOwnership::Owned,
        )
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn register_borrowed_cet_default_shadow_stack(
        &mut self,
        task_id: u32,
        start: VirtAddr,
        size: usize,
    ) -> AxResult {
        self.register_cet_default_shadow_stack_with_ownership(
            task_id,
            start,
            size,
            CetDefaultShadowStackOwnership::Borrowed,
        )
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn register_cet_default_shadow_stack_with_ownership(
        &mut self,
        task_id: u32,
        start: VirtAddr,
        size: usize,
        ownership: CetDefaultShadowStackOwnership,
    ) -> AxResult {
        let mut extents = Vec::new();
        extents.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        extents.push(CetDefaultShadowStackExtent { start, size });
        self.register_cet_default_shadow_stack_extents_with_ownership(task_id, extents, ownership)
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn register_cet_default_shadow_stack_extents(
        &mut self,
        task_id: u32,
        extents: Vec<CetDefaultShadowStackExtent>,
    ) -> AxResult {
        self.register_cet_default_shadow_stack_extents_with_ownership(
            task_id,
            extents,
            CetDefaultShadowStackOwnership::Owned,
        )
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn register_borrowed_cet_default_shadow_stack_extents(
        &mut self,
        task_id: u32,
        extents: Vec<CetDefaultShadowStackExtent>,
    ) -> AxResult {
        self.register_cet_default_shadow_stack_extents_with_ownership(
            task_id,
            extents,
            CetDefaultShadowStackOwnership::Borrowed,
        )
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn register_cet_default_shadow_stack_extents_with_ownership(
        &mut self,
        task_id: u32,
        mut extents: Vec<CetDefaultShadowStackExtent>,
        ownership: CetDefaultShadowStackOwnership,
    ) -> AxResult {
        Self::normalize_cet_default_shadow_stack_extents(&mut extents);
        if extents.is_empty()
            || self
                .cet_default_shadow_stacks
                .iter()
                .any(|owner| owner.task_id == task_id)
            || extents.iter().any(|extent| {
                extent.size == 0 || !self.cet_shadow_stack_extent_covers(extent.start, extent.size)
            })
        {
            return Err(AxError::InvalidInput);
        }
        self.cet_default_shadow_stacks
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        self.cet_default_shadow_stacks
            .push(CetDefaultShadowStackOwner {
                task_id,
                start: extents[0].start,
                size: extents[0].size,
                extents,
                ownership,
            });
        Ok(())
    }

    /// Removes one owner without touching VMAs.  Exec uses this before the
    /// image handoff; exit uses `detach_*` below to remove its private VMA.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn take_cet_default_shadow_stack(
        &mut self,
        task_id: u32,
    ) -> Option<CetDefaultShadowStackOwner> {
        let index = self
            .cet_default_shadow_stacks
            .iter()
            .position(|owner| owner.task_id == task_id)?;
        Some(self.cet_default_shadow_stacks.swap_remove(index))
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn cet_default_shadow_stack(
        &self,
        task_id: u32,
    ) -> Option<CetDefaultShadowStackOwner> {
        self.cet_default_shadow_stacks
            .iter()
            .cloned()
            .find(|owner| owner.task_id == task_id)
    }

    /// Retires one automatic-stack cleanup record exactly once. Borrowed
    /// vfork aliases name the parent's VMA and therefore only lose the alias.
    /// A peer may already have unmapped an owned VMA; that is still a complete
    /// cleanup operation and must not leave a stale record that blocks a later
    /// disable/re-enable cycle.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn retire_cet_default_shadow_stack(&mut self, task_id: u32) -> DeferredUffdWake {
        let Some(owner) = self.take_cet_default_shadow_stack(task_id) else {
            return DeferredUffdWake::empty();
        };
        if owner.ownership == CetDefaultShadowStackOwnership::Owned {
            // The record is cleanup metadata, never SSP authority. Drop it
            // before attempting the best-effort VMA retirement so a peer
            // munmap cannot strand CET state behind stale ownership.
            let mut wake = DeferredUffdWake::empty();
            // The registry is removed before touching VMAs, so every extent
            // is reclaimed at most once even if a peer has already removed
            // some fragments. Explicit map_shadow_stack VMAs never acquire a
            // registry extent and therefore cannot be selected here.
            for extent in owner.extents {
                if let Ok(fragment_wake) = self.unmap(extent.start, extent.size) {
                    wake.merge(fragment_wake);
                }
            }
            wake
        } else {
            DeferredUffdWake::empty()
        }
    }

    /// Tests whether an SSP names the end of a live user shadow-stack word.
    ///
    /// This deliberately does not consult the default-stack owner registry:
    /// owner records exist solely to reclaim automatically allocated stacks.
    /// Users may pivot SSP to an explicit `map_shadow_stack(2)` mapping.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn cet_shadow_stack_pointer_valid(&self, ssp: u64) -> bool {
        let Some(start) = (ssp as usize).checked_sub(core::mem::size_of::<u64>()) else {
            return false;
        };
        ssp.is_multiple_of(core::mem::size_of::<u64>() as u64)
            && self
                .cet_shadow_stack_extent_covers(VirtAddr::from(start), core::mem::size_of::<u64>())
    }

    /// Validates one resident, kernel-authorized CET write span while the MM
    /// lock is held.  Unlike a generic readable span, this accepts only an
    /// exact user shadow-stack VMA and matching resident SHSTK PTEs; it is the
    /// nofault transaction gate for paired normal/shadow-stack updates.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn cet_shadow_stack_span_resident(&self, start: usize, len: usize) -> bool {
        if !matches!(len, 4 | 8 | 32)
            || start.checked_add(len).is_none()
            || !self.cet_shadow_stack_extent_covers(VirtAddr::from(start), len)
        {
            return false;
        }
        let end = start + len;
        let first = start & !(PAGE_SIZE_4K - 1);
        let last = (end - 1) & !(PAGE_SIZE_4K - 1);
        for page in [first, last] {
            let Some(area) = self.find_area(VirtAddr::from(page)) else {
                return false;
            };
            let vma = area.flags();
            // Linux keeps SHSTK's kernel-authorized write policy in the VMA
            // (and strips ordinary user write from the leaf encoding).  Do
            // not reject that required VMA WRITE bit; the SHSTK PTE identity
            // below is what prevents this from becoming generic usercopy.
            if !vma.contains(MappingFlags::USER | MappingFlags::READ | MappingFlags::SHADOW_STACK)
                || vma.contains(MappingFlags::EXECUTE)
            {
                return false;
            }
            let Ok((_, pte, size)) = self.page_table().query(VirtAddr::from(page)) else {
                return false;
            };
            if size != PageSize::Size4K
                || !pte.contains(MappingFlags::USER | MappingFlags::SHADOW_STACK)
                || pte.intersects(MappingFlags::EXECUTE)
            {
                return false;
            }
        }
        true
    }

    /// Tests whether a Linux x86 signal transition can push its restorer and
    /// restore token below `ssp` without leaving a live SHSTK mapping.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn cet_signal_frame_fits(&self, ssp: u64) -> bool {
        let bytes = core::mem::size_of::<[u64; 2]>();
        let Some(start) = (ssp as usize).checked_sub(bytes) else {
            return false;
        };
        ssp.is_multiple_of(core::mem::size_of::<u64>() as u64)
            && self.cet_shadow_stack_extent_covers(VirtAddr::from(start), bytes)
    }

    /// A borrowed vfork stack deliberately shares its physical leaves with
    /// the suspended parent. Its CET writes must therefore retain the SHSTK
    /// PTE rather than taking the ordinary fork-COW write path.
    #[cfg(target_arch = "x86_64")]
    pub(super) fn borrowed_cet_shadow_stack_contains(&self, address: VirtAddr) -> bool {
        self.cet_default_shadow_stacks.iter().any(|owner| {
            owner.ownership == CetDefaultShadowStackOwnership::Borrowed
                && owner.extents.iter().any(|extent| {
                    extent.start <= address && extent.end().is_some_and(|end| address < end)
                })
        })
    }

    /// Performs the all-or-nothing kernel side of a CET signal push.  The
    /// VMA and every touched leaf are validated before any word is written;
    /// SHADOW_STACK mappings are intentionally written through this narrow
    /// kernel authority rather than general usercopy.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn write_cet_signal_frame(
        &mut self,
        saved_ssp: u64,
        words: [u64; 2],
    ) -> AxResult<u64> {
        let bytes = core::mem::size_of_val(&words);
        let start = (saved_ssp as usize)
            .checked_sub(bytes)
            .ok_or(AxError::BadAddress)?;
        if !self.cet_signal_frame_fits(saved_ssp) {
            return Err(AxError::BadAddress);
        }
        let first_page = start & !(PAGE_SIZE_4K - 1);
        let last_page = (saved_ssp as usize - 1) & !(PAGE_SIZE_4K - 1);
        let population = last_page
            .checked_sub(first_page)
            .and_then(|delta| delta.checked_add(PAGE_SIZE_4K))
            .ok_or(AxError::BadAddress)?;
        // This is the kernel-authorized equivalent of a CET write: it must
        // fault a fork-demoted RO COW leaf privately and restore its SHSTK
        // PTE encoding, without granting ordinary user WRITE permission.
        let populate_flags = if self.borrowed_cet_shadow_stack_contains(VirtAddr::from(start)) {
            MappingFlags::SHADOW_STACK
        } else {
            MappingFlags::SHADOW_STACK | MappingFlags::WRITE
        };
        self.populate_area(VirtAddr::from(first_page), population, populate_flags)?;
        for address in [start, saved_ssp as usize - core::mem::size_of::<u64>()] {
            let (_, flags, _) = self.page_table().query(VirtAddr::from(address))?;
            if !flags.contains(MappingFlags::USER) || !flags.contains(MappingFlags::SHADOW_STACK) {
                return Err(AxError::BadAddress);
            }
        }
        let mut bytes = [0u8; core::mem::size_of::<[u64; 2]>()];
        for (index, word) in words.into_iter().enumerate() {
            bytes[index * 8..(index + 1) * 8].copy_from_slice(&word.to_ne_bytes());
        }
        self.write(VirtAddr::from(start), &bytes)?;
        Ok(start as u64)
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn read_cet_signal_frame(&self, shadow_start: u64) -> AxResult<[u64; 2]> {
        let _end = shadow_start
            .checked_add(core::mem::size_of::<[u64; 2]>() as u64)
            .ok_or(AxError::BadAddress)?;
        if !shadow_start.is_multiple_of(core::mem::size_of::<u64>() as u64)
            || !self.cet_shadow_stack_extent_covers(
                VirtAddr::from(shadow_start as usize),
                core::mem::size_of::<[u64; 2]>(),
            )
        {
            return Err(AxError::BadAddress);
        }
        let mut bytes = [0u8; core::mem::size_of::<[u64; 2]>()];
        self.read(VirtAddr::from(shadow_start as usize), &mut bytes)?;
        let mut words = [0u64; 2];
        for (index, word) in words.iter_mut().enumerate() {
            *word = u64::from_ne_bytes(bytes[index * 8..(index + 1) * 8].try_into().unwrap());
        }
        Ok(words)
    }

    /// Reads the token consumed by a signal restorer. At this point the
    /// handler's RET already consumed the preceding restorer word, so `ssp`
    /// names the token itself rather than a complete signal frame.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn read_cet_signal_restore_token(&self, ssp: u64) -> AxResult<u64> {
        let bytes = core::mem::size_of::<u64>();
        if !ssp.is_multiple_of(bytes as u64)
            || !self.cet_shadow_stack_extent_covers(VirtAddr::from(ssp as usize), bytes)
        {
            return Err(AxError::BadAddress);
        }
        let mut raw = [0u8; core::mem::size_of::<u64>()];
        self.read(VirtAddr::from(ssp as usize), &mut raw)?;
        Ok(u64::from_ne_bytes(raw))
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn normalize_cet_default_shadow_stack_extents(
        extents: &mut Vec<CetDefaultShadowStackExtent>,
    ) {
        extents.retain(|extent| extent.size != 0 && extent.end().is_some());
        extents.sort_unstable_by_key(|extent| extent.start);
        let mut output = 0;
        for index in 0..extents.len() {
            let extent = extents[index];
            if output != 0
                && extents[output - 1]
                    .end()
                    .is_some_and(|end| extent.start <= end)
            {
                let previous = &mut extents[output - 1];
                let end = previous
                    .end()
                    .expect("normalized CET extent has a checked end")
                    .max(
                        extent
                            .end()
                            .expect("normalized CET extent has a checked end"),
                    );
                previous.size = end.sub_addr(previous.start);
            } else {
                extents[output] = extent;
                output += 1;
            }
        }
        extents.truncate(output);
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn refresh_cet_default_shadow_stack_bounds(owner: &mut CetDefaultShadowStackOwner) {
        if let Some(first) = owner.extents.first().copied() {
            owner.start = first.start;
            owner.size = first.size;
        } else {
            owner.start = VirtAddr::from(0);
            owner.size = 0;
        }
    }

    /// Remove only the automatic-stack portions invalidated by a peer VMA
    /// removal. This happens at the mutation boundary, before a later
    /// explicit map_shadow_stack can reuse the same address; retirement can
    /// therefore never mistake that new explicit mapping for an old automatic
    /// extent.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn remove_cet_default_shadow_stack_extents_for_unmap(
        &mut self,
        start: VirtAddr,
        size: usize,
    ) {
        let Some(end) = start.checked_add(size) else {
            return;
        };
        for owner in &mut self.cet_default_shadow_stacks {
            let mut output = 0;
            for index in 0..owner.extents.len() {
                let extent = owner.extents[index];
                let extent_end = extent
                    .end()
                    .expect("registered CET extent has a checked end");
                if extent_end <= start || end <= extent.start {
                    owner.extents[output] = extent;
                    output += 1;
                    continue;
                }
                if extent.start < start {
                    owner.extents[output] = CetDefaultShadowStackExtent {
                        start: extent.start,
                        size: start.sub_addr(extent.start),
                    };
                    output += 1;
                }
                if end < extent_end {
                    owner.extents[output] = CetDefaultShadowStackExtent {
                        start: end,
                        size: extent_end.sub_addr(end),
                    };
                    output += 1;
                }
            }
            owner.extents.truncate(output);
            Self::refresh_cet_default_shadow_stack_bounds(owner);
        }
    }

    /// Reserve fragmented-owner capacity before an mremap transaction can
    /// publish the corresponding VMA topology. Rebase below is then strictly
    /// infallible and cannot leave committed mappings with stale cleanup
    /// ownership.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn prepare_cet_default_shadow_stacks_for_mremap(
        &mut self,
        source: VirtAddr,
        old_size: usize,
        duplicate: bool,
    ) -> AxResult {
        let Some(source_end) = source.checked_add(old_size) else {
            return Err(AxError::InvalidInput);
        };
        for owner in &mut self.cet_default_shadow_stacks {
            let intersecting = owner
                .extents
                .iter()
                .filter(|extent| {
                    extent
                        .end()
                        .is_some_and(|end| extent.start < source_end && source < end)
                })
                .count();
            // A duplicate adds one destination extent per intersected extent.
            // A move replaces each intersected extent, with at most two extra
            // source-side boundary fragments overall.
            let additional = if duplicate {
                intersecting
            } else {
                intersecting.saturating_add(2)
            };
            owner
                .extents
                .try_reserve(additional)
                .map_err(|_| AxError::NoMemory)?;
        }
        Ok(())
    }

    /// Rebase default-stack cleanup records after mremap while still inside
    /// its VMA transaction. These records are not active-SSP authorization:
    /// a peer mutation is made visible by the synchronized TLB update and any
    /// subsequent CET access faults architecturally.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn rebase_cet_default_shadow_stacks_after_mremap(
        &mut self,
        source: VirtAddr,
        old_size: usize,
        new_size: usize,
        destination: VirtAddr,
        duplicate: bool,
    ) {
        let Some(source_end) = source.checked_add(old_size) else {
            return;
        };
        let moved_size = old_size.min(new_size);
        let Some(moved_source_end) = source.checked_add(moved_size) else {
            return;
        };
        for owner in &mut self.cet_default_shadow_stacks {
            let original_len = owner.extents.len();
            for index in 0..original_len {
                let extent = owner.extents[index];
                let extent_end = extent
                    .end()
                    .expect("registered CET extent has a checked end");
                let overlap_start = extent.start.max(source);
                let overlap_end = extent_end.min(moved_source_end);
                if overlap_start >= overlap_end {
                    continue;
                }
                if duplicate {
                    let offset = overlap_start.sub_addr(source);
                    if let Some(start) = destination.checked_add(offset) {
                        owner.extents.push(CetDefaultShadowStackExtent {
                            start,
                            size: overlap_end.sub_addr(overlap_start),
                        });
                    }
                    continue;
                }

                // Replace this source fragment with its surviving left part;
                // append the moved middle and surviving right part. This
                // handles a source range beginning or ending in the interior
                // of an owner extent rather than only rebasing owner.start.
                let mut replacement = [None, None, None];
                if extent.start < overlap_start {
                    replacement[0] = Some(CetDefaultShadowStackExtent {
                        start: extent.start,
                        size: overlap_start.sub_addr(extent.start),
                    });
                }
                let offset = overlap_start.sub_addr(source);
                if let Some(start) = destination.checked_add(offset) {
                    replacement[1] = Some(CetDefaultShadowStackExtent {
                        start,
                        size: overlap_end.sub_addr(overlap_start),
                    });
                }
                if overlap_end < extent_end {
                    replacement[2] = Some(CetDefaultShadowStackExtent {
                        start: overlap_end,
                        size: extent_end.sub_addr(overlap_end),
                    });
                }
                let mut replacements = replacement.into_iter().flatten();
                if let Some(first) = replacements.next() {
                    owner.extents[index] = first;
                    owner.extents.extend(replacements);
                } else {
                    owner.extents[index].size = 0;
                }
            }
            // A shrinking move removes the old source tail. Its overlap lies
            // outside `moved_source_end`, so drop it after relocating the
            // prefix above. Duplication never removes source bytes.
            if !duplicate && moved_size < old_size {
                let tail_start = moved_source_end;
                let mut output = 0;
                for index in 0..owner.extents.len() {
                    let extent = owner.extents[index];
                    let extent_end = extent
                        .end()
                        .expect("registered CET extent has a checked end");
                    if extent_end <= tail_start || source_end <= extent.start {
                        owner.extents[output] = extent;
                        output += 1;
                    } else {
                        if extent.start < tail_start {
                            owner.extents[output] = CetDefaultShadowStackExtent {
                                start: extent.start,
                                size: tail_start.sub_addr(extent.start),
                            };
                            output += 1;
                        }
                        if source_end < extent_end {
                            owner.extents[output] = CetDefaultShadowStackExtent {
                                start: source_end,
                                size: extent_end.sub_addr(source_end),
                            };
                            output += 1;
                        }
                    }
                }
                owner.extents.truncate(output);
            }
            Self::normalize_cet_default_shadow_stack_extents(&mut owner.extents);
            Self::refresh_cet_default_shadow_stack_bounds(owner);
        }
    }

    pub(crate) fn shared_backing_key_at(&self, address: VirtAddr) -> Option<SharedBackingKey> {
        self.find_area(address)?.backend().shared_backing_key()
    }

    pub(crate) fn shared_pages_at(&self, address: VirtAddr) -> Option<Arc<SharedPages>> {
        self.find_area(address)?.backend().shared_pages().cloned()
    }

    pub(crate) fn shared_backing_offset_at(&self, address: VirtAddr) -> Option<usize> {
        match self.find_area(address)?.backend() {
            Backend::Shared(shared) => shared.backing_offset(address.as_usize()),
            Backend::Linear(_) | Backend::Cow(_) | Backend::File(_) => None,
        }
    }

    /// Returns only VMAs intersecting `range`, starting from the crossing
    /// predecessor instead of walking the complete address-space prefix.
    pub(crate) fn areas_overlapping(
        &self,
        range: VirtAddrRange,
    ) -> impl Iterator<Item = &MemoryArea<Backend>> {
        self.areas.iter_overlapping(range)
    }
}
