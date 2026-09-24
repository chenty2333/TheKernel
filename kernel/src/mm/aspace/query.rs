//! AddrSpace: locked ranges, pins, mapping snapshots, accounting and free-area search.

use super::*;

impl AddrSpace {
    pub fn locked_bytes(&self) -> usize {
        self.locked_ranges
            .iter()
            .map(|(start, end)| end.sub_addr(*start))
            .sum()
    }

    pub fn locked_bytes_in_range(&self, start: VirtAddr, size: usize) -> usize {
        if size == 0 {
            return 0;
        }
        let end = start + size;
        self.locked_ranges
            .range(..end)
            .filter_map(|(&range_start, &range_end)| {
                if range_end <= start {
                    return None;
                }
                let overlap_start = range_start.max(start);
                let overlap_end = range_end.min(end);
                (overlap_start < overlap_end).then_some(overlap_end.sub_addr(overlap_start))
            })
            .sum()
    }

    pub fn locked_segments_in_range(&self, start: VirtAddr, size: usize) -> Vec<(VirtAddr, usize)> {
        if size == 0 {
            return Vec::new();
        }
        let end = start + size;
        self.locked_ranges
            .range(..end)
            .filter_map(|(&range_start, &range_end)| {
                if range_end <= start {
                    return None;
                }
                let overlap_start = range_start.max(start);
                let overlap_end = range_end.min(end);
                (overlap_start < overlap_end)
                    .then_some((overlap_start, overlap_end.sub_addr(overlap_start)))
            })
            .collect()
    }

    pub fn range_is_fully_locked(&self, start: VirtAddr, size: usize) -> bool {
        size > 0 && self.locked_bytes_in_range(start, size) == size
    }

    pub(crate) fn user_io_pin_owner(&self) -> PinOwner {
        PinOwner::new(self.address_space_id.get()).expect("address-space IDs are nonzero")
    }

    pub(crate) fn begin_user_io_pin(
        &mut self,
        request: PinRequest,
    ) -> AxResult<(PinReservation, UserIoSystemPinCharge)> {
        if request.owner() != self.user_io_pin_owner() {
            return Err(AxError::InvalidInput);
        }
        let system_charge = UserIoSystemPinCharge::reserve(request)?;
        let reservation = self
            .user_io_pins
            .reserve(request, self.address_space_id)
            .map_err(mm_error)?;
        Ok((reservation, system_charge))
    }

    pub(crate) fn cancel_user_io_pin(&mut self, reservation: PinReservation) {
        // `revalidate_next` removes a stale reservation itself. Treat that
        // already-rolled-back state as successful cancellation so adapter RAII
        // can use one cleanup path for every preparation failure.
        if self.user_io_pins.view(reservation.token()).is_err() {
            return;
        }
        if let Err(error) = self.user_io_pins.cancel_reservation(reservation) {
            debug!(
                "AddrSpace::cancel_user_io_pin: token {}: {error:?}",
                reservation.token().get()
            );
        }
    }

    /// Revalidates one caller-bounded window of a reserved user-I/O pin.
    ///
    /// The reservation was published before any expectation or lower owner was
    /// collected, and every overlapping mapping mutation consults
    /// `user_io_pins`. It is therefore the range mutation fence between these
    /// short address-space lock acquisitions. `expectations` must begin at
    /// `start`, remain inside this window, and cover it without a gap; the
    /// return value tells the caller how many entries to remove from its
    /// remaining prefix.
    pub(crate) fn revalidate_user_io_pin_window(
        &mut self,
        reservation: PinReservation,
        expectations: &[UserIoMappingExpectation],
        start: VirtAddr,
        size: usize,
    ) -> AxResult<usize> {
        self.validate_region(start, size)?;
        if size == 0 {
            return Err(AxError::InvalidInput);
        }
        let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
        let mut cursor = start;
        let mut consumed = 0usize;
        for expectation in expectations {
            if VirtAddr::from(expectation.covered.start()) != cursor
                || expectation.covered.end() > end.as_usize()
            {
                return Err(AxError::BadState);
            }
            let area = self.areas.find(cursor).ok_or(AxError::BadAddress)?;
            let current = self.mapping_snapshot(area)?;
            self.user_io_pins
                .revalidate_next(
                    reservation,
                    expectation.expected,
                    current,
                    expectation.covered,
                )
                .map_err(mm_error)?;
            cursor = VirtAddr::from(expectation.covered.end());
            consumed = consumed.checked_add(1).ok_or(AxError::NoMemory)?;
            if cursor == end {
                break;
            }
        }
        if cursor != end {
            return Err(AxError::BadState);
        }
        Ok(consumed)
    }

    /// Turns a fully revalidated reservation into an active lease.
    ///
    /// Every per-VMA operation has already completed in bounded windows, so
    /// this final address-space critical section is constant-time.
    pub(crate) fn commit_user_io_pin(
        &mut self,
        reservation: PinReservation,
        cow_frames: &mut Vec<PhysAddr>,
    ) -> AxResult<PinToken> {
        let request = self
            .user_io_pins
            .view(reservation.token())
            .map_err(mm_error)?
            .request();
        let tracks_cow_frames = request.duration() == tk_linux_mm::PinDuration::LongTerm
            && request.access() == tk_linux_mm::PinAccess::Write
            && !cow_frames.is_empty();
        if tracks_cow_frames
            && self.active_long_term_cow_pins.len() == self.active_long_term_cow_pins.capacity()
        {
            return Err(AxError::ResourceBusy);
        }

        let token = self.user_io_pins.commit(reservation).map_err(mm_error)?;
        if tracks_cow_frames {
            self.active_long_term_cow_pins.push(ActiveLongTermCowPin {
                token,
                frames: core::mem::take(cow_frames),
            });
        }
        Ok(token)
    }

    pub(crate) fn end_user_io_pin(&mut self, token: PinToken) -> Option<Vec<PhysAddr>> {
        if let Err(error) = self.user_io_pins.release(token) {
            debug!(
                "AddrSpace::end_user_io_pin: token {}: {error:?}",
                token.get()
            );
            return None;
        }
        self.active_long_term_cow_pins
            .iter()
            .position(|pin| pin.token == token)
            .map(|index| self.active_long_term_cow_pins.swap_remove(index).frames)
    }

    pub(super) fn active_long_term_cow_frames(&self) -> AxResult<Vec<PhysAddr>> {
        let count = self
            .active_long_term_cow_pins
            .iter()
            .try_fold(0usize, |count, pin| count.checked_add(pin.frames.len()))
            .ok_or(AxError::NoMemory)?;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        for pin in &self.active_long_term_cow_pins {
            frames.extend_from_slice(&pin.frames);
        }
        frames.sort_unstable();
        frames.dedup();
        Ok(frames)
    }

    pub fn user_io_pin_overlaps(&self, start: VirtAddr, size: usize) -> bool {
        self.invalidation(start, size, InvalidationReason::Unmap)
            .map_or(true, |invalidation| {
                self.user_io_pins
                    .first_mutation_blocker(invalidation)
                    .is_some()
            })
    }

    pub(super) fn check_no_user_io_pin_overlap(
        &self,
        start: VirtAddr,
        size: usize,
        reason: InvalidationReason,
    ) -> AxResult {
        let invalidation = self.invalidation(start, size, reason)?;
        self.user_io_pins
            .admit_mutation(invalidation)
            .map_err(mm_error)
    }

    /// Appends mapping expectations for one caller-bounded scan window.
    ///
    /// `snapshots` must reserve at least one slot per covered page before the
    /// caller takes the address-space lock. VMAs are page-aligned, so this
    /// method can then split a VMA at the window boundary and push without any
    /// allocation. Partial output on error remains owned by unpublished-pin
    /// RAII and must be dropped only after the caller releases the lock.
    pub(crate) fn append_user_io_mapping_expectations(
        &self,
        start: VirtAddr,
        size: usize,
        access_flags: MappingFlags,
        snapshots: &mut Vec<UserIoMappingExpectation>,
    ) -> AxResult {
        self.validate_region(start, size)?;
        if size == 0 {
            return Err(AxError::InvalidInput);
        }
        let page_count = size / PAGE_SIZE_4K;
        if snapshots.capacity().saturating_sub(snapshots.len()) < page_count {
            return Err(AxError::NoMemory);
        }

        let mut cursor = start;
        let end = start + size;
        while cursor < end {
            let area = self.areas.find(cursor).ok_or(AxError::BadAddress)?;
            if area.start() > cursor || !area.flags().contains(access_flags) {
                return Err(AxError::BadAddress);
            }
            let segment_end = area.end().min(end);
            let covered = PageRange::new(
                cursor.as_usize(),
                segment_end.sub_addr(cursor),
                PAGE_SIZE_4K,
            )
            .map_err(mm_error)?;
            snapshots.push(UserIoMappingExpectation {
                expected: self.mapping_snapshot(area)?.expected(),
                covered,
                needs_frame_registry: area.backend().supports_user_io_frame_pin(),
            });
            cursor = segment_end;
        }
        Ok(())
    }

    pub(super) fn mapping_snapshot(&self, area: &MemoryArea<Backend>) -> AxResult<MappingSnapshot> {
        Self::mapping_snapshot_from_parts(self.address_space_id, &self.mapping_identities, area)
    }

    pub(super) fn uffd_resolver_area_in(
        areas: &MemorySet<Backend>,
        destination: PageRange,
    ) -> AxResult<&MemoryArea<Backend>> {
        if destination.page_size().bytes() != PAGE_SIZE_4K {
            return Err(AxError::InvalidInput);
        }
        let start = VirtAddr::from(destination.start());
        let area = areas
            .find(start)
            .ok_or_else(|| AxError::from(axerrno::LinuxError::ENOENT))?;
        if area.start() > start
            || area.end().as_usize() < destination.end()
            || !area.backend().supports_uffd_missing_resolver()
        {
            return Err(AxError::from(axerrno::LinuxError::ENOENT));
        }
        Ok(area)
    }

    /// Freezes whole-range COPY/ZEROPAGE authority before any page or
    /// page-table allocation.
    ///
    /// Linux requires the complete destination to remain within one
    /// compatible registered VMA. The returned lease is revalidated for every
    /// page publication, allowing the long-running ioctl to release the
    /// address-space mutex between pages without weakening that rule.
    pub(crate) fn preflight_uffd_resolver_range(
        &mut self,
        destination: PageRange,
    ) -> AxResult<UffdResolverLease> {
        self.ensure_4k_granularity(VirtAddr::from(destination.start()), destination.len())?;
        let area = Self::uffd_resolver_area_in(&self.areas, destination)?;
        let mapping = self.mapping_snapshot(area)?;
        self.uffd
            .as_ref()
            .ok_or_else(|| AxError::from(axerrno::LinuxError::ENOENT))?
            .prepare_resolver(mapping, destination)
    }

    /// Publishes one fully initialized resolver page with a short
    /// address-space critical section.
    ///
    /// All allocation, source usercopy, unused-owner reclamation, executable
    /// synchronization, signed-result copyout, and waiter wake remain outside
    /// this method. Under the mutex it only revalidates VMA/registration/PTE
    /// state, publishes one prepared leaf, and records an immutable deferred
    /// broker completion.
    pub(crate) fn publish_prepared_uffd_page(
        &mut self,
        lease: UffdResolverLease,
        page: PageRange,
        disposition: FaultDisposition,
        prepared: &mut PreparedCowPage,
        icache_synchronization: Option<UffdIcacheSynchronization>,
    ) -> AxResult<UffdPagePublication> {
        if page.len() != PAGE_SIZE_4K || !lease.destination().contains(page) {
            return Err(AxError::InvalidInput);
        }

        let Self {
            address_space_id,
            areas,
            mapping_identities,
            uffd,
            pt,
            ..
        } = self;
        let area = Self::uffd_resolver_area_in(areas, lease.destination())?;
        let mapping =
            Self::mapping_snapshot_from_parts(*address_space_id, mapping_identities, area)?;
        let state = uffd
            .as_mut()
            .ok_or_else(|| AxError::from(axerrno::LinuxError::ENOENT))?;
        let current = state.revalidate_resolver(lease, mapping)?;
        let flags = area.flags();
        if flags.contains(MappingFlags::EXECUTE) && icache_synchronization.is_none() {
            return Ok(UffdPagePublication::NeedsIcacheSynchronization);
        }
        let completions = state.validate_resolver_completion(lease, current, page, disposition)?;
        area.backend().publish_prepared_cow_page(
            VirtAddr::from(page.start()),
            flags,
            pt,
            prepared,
        )?;
        state.defer_resolver_completions(completions);
        Ok(UffdPagePublication::Published)
    }

    /// Applies one handler-scoped UFFDIO_WAKE transition.
    ///
    /// The returned receipt owns every PollSet wake and must be finished only
    /// after the caller releases the address-space mutex.
    pub(crate) fn wake_uffd_handler_range(
        &mut self,
        handler: FaultHandlerId,
        range: PageRange,
    ) -> AxResult<DeferredUffdWake> {
        self.uffd
            .as_mut()
            .ok_or(AxError::BadState)?
            .wake_handler_range(handler, range)
    }

    /// Returns the current mapping snapshot only when `address` is covered by
    /// a VMA and its 4 KiB page-table leaf is still absent.
    ///
    /// This is a generic, read-only admission primitive. It does not interpret
    /// Linux userfaultfd registration or allocate page-table state; the
    /// adapter performs that policy check separately while retaining the same
    /// address-space lock.
    pub(in crate::mm) fn missing_mapping_snapshot_at(
        &self,
        address: VirtAddr,
    ) -> AxResult<Option<MappingSnapshot>> {
        if !self.va_range.contains(address) {
            return Ok(None);
        }
        let Some(area) = self.areas.find(address) else {
            return Ok(None);
        };
        let page = address.align_down(PAGE_SIZE_4K);
        // A swap PTE is a recoverable resident image, not a UFFD MISSING
        // hole.  Let normal fault handling page it in rather than allowing a
        // resolver to overwrite it with zeroes or unrelated copied bytes.
        if self.swapped.contains_key(&page) {
            return Ok(None);
        }
        match self.pt.query(page) {
            Ok(_) => Ok(None),
            Err(PagingError::NotMapped) => self.mapping_snapshot(area).map(Some),
            Err(_) => Err(AxError::BadState),
        }
    }

    pub(super) fn mapping_snapshot_from_parts(
        address_space_id: AddressSpaceId,
        mapping_identities: &MappingIdentityIndex,
        area: &MemoryArea<Backend>,
    ) -> AxResult<MappingSnapshot> {
        let flags = area.flags();
        let identity = mapping_identity(mapping_identities, area.lineage())?;
        let range =
            PageRange::new(area.start().as_usize(), area.size(), PAGE_SIZE_4K).map_err(mm_error)?;
        let (long_term_pinnable, writable_file_pin_supported) =
            mapping_user_io_pin_policy(area.backend());
        Ok(MappingSnapshot::new(
            address_space_id,
            identity.id,
            identity.generation,
            range,
            MappingAccess::new(
                flags.contains(MappingFlags::READ),
                flags.contains(MappingFlags::WRITE),
                flags.contains(MappingFlags::EXECUTE),
            ),
            area.backend().linux_mapping_kind(),
            long_term_pinnable,
            writable_file_pin_supported,
        ))
    }

    pub(super) fn projected_protect_piece_at<'a, B: memory_set::MappingBackend>(
        areas: &'a MemorySet<B>,
        protect: memory_addr::AddrRange<B::Addr>,
        ranges: &[PreparedProtectRange<B::Addr, B::Flags>],
        address: B::Addr,
    ) -> Option<ProjectedProtectPiece<'a, B>> {
        let area = areas.find(address)?;
        let (start, end, flags) = if address < protect.start {
            (area.start(), area.end().min(protect.start), area.flags())
        } else if address < protect.end {
            (
                area.start().max(protect.start),
                area.end().min(protect.end),
                ranges
                    .iter()
                    .find(|range| range.start <= address && address < range.end)
                    .expect("prepared protection ranges cover projected UFFD fragment")
                    .flags,
            )
        } else {
            (area.start().max(protect.end), area.end(), area.flags())
        };
        (start < end).then_some(ProjectedProtectPiece {
            area,
            start,
            end,
            flags,
        })
    }

    /// Projects MemorySet's exact post-mprotect merge law without mutating the
    /// area tree or allocating. The scan replays the retained-left merge law
    /// for the final structurally compatible run containing `address`.
    pub(super) fn projected_protect_run_at_ranges<'a, B: memory_set::MappingBackend>(
        areas: &'a MemorySet<B>,
        protect: memory_addr::AddrRange<B::Addr>,
        ranges: &[PreparedProtectRange<B::Addr, B::Flags>],
        address: B::Addr,
    ) -> Option<ProjectedProtectRun<'a, B>> {
        let mut anchor = Self::projected_protect_piece_at(areas, protect, ranges, address)?;

        // Backend compatibility is deliberately not a left-scan barrier.
        // MemorySet processes protection actions in ascending address order,
        // retains the left backend after a merge, and starts a new run after
        // an incompatible pair. Replaying from the first structurally
        // compatible piece is therefore necessary because `can_merge` is not
        // required to be transitive.
        while Into::<usize>::into(anchor.start) != 0 {
            let previous_address = B::Addr::from(Into::<usize>::into(anchor.start) - 1);
            let Some(previous) =
                Self::projected_protect_piece_at(areas, protect, ranges, previous_address)
            else {
                break;
            };
            if !projected_protect_pieces_share_structure(&previous, &anchor) {
                break;
            }
            anchor = previous;
        }

        let mut run = ProjectedProtectRun {
            left_area: anchor.area,
            start: anchor.start,
            end: anchor.end,
            flags: anchor.flags,
        };
        loop {
            let Some(next) = Self::projected_protect_piece_at(areas, protect, ranges, run.end)
            else {
                return (run.start <= address && address < run.end).then_some(run);
            };
            // MemorySet retains the left/current area when it absorbs a right
            // neighbor. Keep comparing that surviving backend against every
            // later neighbor; `can_merge` is not required to be transitive.
            let survivor = ProjectedProtectPiece {
                area: run.left_area,
                start: run.start,
                end: run.end,
                flags: run.flags,
            };
            if projected_protect_pieces_merge(&survivor, &next) {
                run.end = next.end;
                continue;
            }
            if run.start <= address && address < run.end {
                return Some(run);
            }
            if !projected_protect_pieces_share_structure(&survivor, &next) {
                return None;
            }
            run = ProjectedProtectRun {
                left_area: next.area,
                start: next.start,
                end: next.end,
                flags: next.flags,
            };
        }
    }

    pub(super) fn projected_protect_run_at<'a, B: memory_set::MappingBackend>(
        areas: &'a MemorySet<B>,
        protect: memory_addr::AddrRange<B::Addr>,
        new_flags: B::Flags,
        address: B::Addr,
    ) -> Option<ProjectedProtectRun<'a, B>> {
        let ranges = [PreparedProtectRange {
            start: protect.start,
            end: protect.end,
            flags: new_flags,
        }];
        Self::projected_protect_run_at_ranges(areas, protect, &ranges, address)
    }

    pub(super) fn projected_uffd_protect_snapshot(
        address_space_id: AddressSpaceId,
        areas: &MemorySet<Backend>,
        mapping_identities: &MappingIdentityIndex,
        protect: VirtAddrRange,
        ranges: &[PreparedProtectRange<VirtAddr, MappingFlags>],
        registration: UffdRegistration,
        fragment: PageRange,
    ) -> AxResult<Option<MappingSnapshot>> {
        let current = Self::uffd_snapshot_for_registration(
            address_space_id,
            areas,
            mapping_identities,
            registration,
        )?;
        let run = Self::projected_protect_run_at_ranges(
            areas,
            protect,
            ranges,
            VirtAddr::from(fragment.start()),
        )
        .ok_or(AxError::BadState)?;
        let post_range = PageRange::new(
            run.start.as_usize(),
            run.end.sub_addr(run.start),
            PAGE_SIZE_4K,
        )
        .map_err(mm_error)?;
        if !post_range.contains(fragment) {
            return Err(AxError::BadState);
        }
        // This is the only `None` proof accepted by the UFFD planner: the
        // complete source remains in one VMA with exactly its old boundaries.
        // Access may change, but it is not registration/fault authority.
        if fragment == registration.range() && post_range == current.range() {
            return Ok(None);
        }

        let identity = mapping_identity(mapping_identities, run.left_area.lineage())?;
        let (long_term_pinnable, writable_file_pin_supported) =
            mapping_user_io_pin_policy(run.left_area.backend());
        let post = MappingSnapshot::new(
            address_space_id,
            identity.id,
            identity.generation,
            post_range,
            MappingAccess::new(
                run.flags.contains(MappingFlags::READ),
                run.flags.contains(MappingFlags::WRITE),
                run.flags.contains(MappingFlags::EXECUTE),
            ),
            run.left_area.backend().linux_mapping_kind(),
            long_term_pinnable,
            writable_file_pin_supported,
        );
        if post.address_space() != registration.address_space()
            || post.mapping() != registration.mapping()
        {
            return Err(AxError::BadState);
        }
        Ok(Some(post))
    }

    pub(super) fn uffd_snapshot_for_registration(
        address_space_id: AddressSpaceId,
        areas: &MemorySet<Backend>,
        mapping_identities: &MappingIdentityIndex,
        registration: UffdRegistration,
    ) -> AxResult<MappingSnapshot> {
        let start = VirtAddr::from(registration.range().start());
        let area = areas.find(start).ok_or(AxError::BadState)?;
        let snapshot =
            Self::mapping_snapshot_from_parts(address_space_id, mapping_identities, area)?;
        if snapshot.address_space() != registration.address_space()
            || snapshot.mapping() != registration.mapping()
            || !snapshot.range().contains(registration.range())
        {
            return Err(AxError::BadState);
        }
        Ok(snapshot)
    }

    /// Appends every mapped VMA intersecting a userfaultfd ioctl range.
    ///
    /// Holes are deliberately skipped. The caller owns a fixed-capacity
    /// scratch vector prepared before this address-space lock was acquired;
    /// this scan never grows it. Compatibility and registration ownership are
    /// validated by the Linux-MM policy layer after the complete scan.
    pub(in crate::mm) fn append_uffd_mapping_snapshots(
        &self,
        range: PageRange,
        snapshots: &mut Vec<MappingSnapshot>,
    ) -> AxResult {
        let scan_range = uffd_vma_scan_range(range, self.base(), self.end())?;
        for area in self.areas.iter_overlapping(scan_range) {
            // Linux's ioctl loops test `vma_can_userfault()` on every VMA they
            // walk and answer `-EINVAL` for the first refusal, before any
            // registration state is touched (`mm/userfaultfd.c:3659-3661`
            // for `UFFDIO_REGISTER`, `:3819-3827` for `UFFDIO_UNREGISTER`).
            // `VM_DROPPABLE` is the first test in that predicate
            // (`mm/userfaultfd.c:2114`): a mapping whose pages may be dropped
            // without a fault cannot promise the handler that it will see
            // every page it did not supply itself.
            if !tk_linux_mm::uffd_can_register_droppable_vma(area.backend().is_droppable()) {
                return Err(AxError::InvalidInput);
            }
            validate_uffd_missing_backend_granule(area.backend().page_size())?;
            if snapshots.len() == snapshots.capacity() {
                return Err(AxError::NoMemory);
            }
            snapshots.push(self.mapping_snapshot(area)?);
        }
        if snapshots.is_empty() {
            return Err(AxError::InvalidInput);
        }
        Ok(())
    }

    pub(super) fn topology_snapshot(&self) -> AxResult<MappingSnapshot> {
        let range =
            PageRange::new(self.base().as_usize(), self.size(), PAGE_SIZE_4K).map_err(mm_error)?;
        Ok(MappingSnapshot::new(
            self.address_space_id,
            self.topology_mapping_id,
            self.topology_generation,
            range,
            MappingAccess::new(true, true, true),
            MappingKind::Special,
            false,
            false,
        ))
    }

    pub(super) fn invalidation(
        &self,
        start: VirtAddr,
        size: usize,
        reason: InvalidationReason,
    ) -> AxResult<InvalidationRange> {
        InvalidationRange::from_raw(self.topology_snapshot()?, start.as_usize(), size, reason)
            .map_err(mm_error)
    }

    pub(super) fn next_topology_generation(&self) -> AxResult<MappingGeneration> {
        self.topology_generation.next().map_err(mm_error)
    }

    pub(super) fn commit_topology_generation(&mut self, next: MappingGeneration) {
        self.topology_generation = next;
    }

    pub fn current_mapping_bytes(&self) -> usize {
        self.areas.iter().map(MemoryArea::size).sum()
    }

    /// Returns the bytes Linux accounts in `mm->data_vm`: the private,
    /// writable, non-grow-down mappings, as
    /// `mm/vma.h:is_data_mapping_vma_flags()` defines them.
    ///
    /// ```c
    /// static inline bool is_data_mapping_vma_flags(const vma_flags_t *vma_flags)
    /// {
    /// 	return vma_flags_test(vma_flags, VMA_WRITE_BIT) &&
    /// 		!vma_flags_test_any(vma_flags, VMA_SHARED_BIT, VMA_STACK_BIT);
    /// }
    /// ```
    ///
    /// Neither tested bit is a hardware permission bit in this kernel, so both
    /// are derived here: `VM_SHARED` from the concrete backend's sharing mode
    /// and `VM_GROWSDOWN` from [`AddrSpace::growdown_starts`].  This is the
    /// accounting `RLIMIT_DATA` is compared against; the kernel keeps no
    /// incremental counter, so the cost is one walk of the area list and only
    /// a caller with a finite `RLIMIT_DATA` pays it.
    pub fn current_data_mapping_bytes(&self) -> usize {
        self.areas
            .iter()
            .filter(|area| {
                is_data_mapping_area(
                    area.flags(),
                    area.backend(),
                    area.start(),
                    &self.growdown_starts,
                )
            })
            .map(MemoryArea::size)
            .sum()
    }

    /// Returns the number of VMA bytes already present in an exact virtual
    /// range.  MAP_FIXED uses this to charge only the net address-space
    /// growth, matching Linux's `pglen - unmapped_pages` accounting.
    pub fn mapped_bytes_in_range(&self, start: VirtAddr, size: usize) -> AxResult<usize> {
        if size == 0 {
            return Ok(0);
        }
        let range = VirtAddrRange::try_from_start_size(start, size).ok_or(AxError::NoMemory)?;
        self.areas
            .iter_overlapping(range)
            .try_fold(0usize, |total, area| {
                let overlap_start = area.start().max(range.start);
                let overlap_end = area.end().min(range.end);
                total
                    .checked_add(overlap_end.sub_addr(overlap_start))
                    .ok_or(AxError::NoMemory)
            })
    }

    /// Returns the `mm->data_vm` bytes already present in an exact virtual
    /// range, filtered the way [`AddrSpace::current_data_mapping_bytes`] does.
    /// A MAP_FIXED replacement's old data mappings are detached before Linux's
    /// `may_expand_vm()` runs, so the `RLIMIT_DATA` arm charges only the net
    /// data growth `length - covered_data_bytes`.
    pub fn data_bytes_in_range(&self, start: VirtAddr, size: usize) -> AxResult<usize> {
        if size == 0 {
            return Ok(0);
        }
        let range = VirtAddrRange::try_from_start_size(start, size).ok_or(AxError::NoMemory)?;
        self.areas
            .iter_overlapping(range)
            .filter(|area| {
                is_data_mapping_area(
                    area.flags(),
                    area.backend(),
                    area.start(),
                    &self.growdown_starts,
                )
            })
            .try_fold(0usize, |total, area| {
                let overlap_start = area.start().max(range.start);
                let overlap_end = area.end().min(range.end);
                total
                    .checked_add(overlap_end.sub_addr(overlap_start))
                    .ok_or(AxError::NoMemory)
            })
    }

    pub fn resident_user_bytes(&self) -> usize {
        self.areas
            .iter()
            .filter(|area| area.flags().contains(MappingFlags::USER))
            // Walk populated page-table branches instead of probing every page
            // in large, sparsely populated stacks and allocator reservations.
            .map(|area| self.pt.mapped_bytes(area.start(), area.size()).unwrap_or(0))
            .sum()
    }

    /// Claims this shared mm's OOM reaper for one PTE generation. Completion
    /// returns to idle: later faults can populate new private pages which a
    /// later process_mrelease must be able to reclaim.
    pub(crate) fn begin_oom_reap(&self) -> AxResult<bool> {
        match self
            .oom_reap_state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(true),
            Err(_) => Err(AxError::ResourceBusy),
        }
    }

    /// Releases the reaper ownership claimed above.
    pub(crate) fn finish_oom_reap(&self) {
        self.oom_reap_state.store(0, Ordering::Release);
    }

    /// Merges an already-observed peak into this mm's high-water mark.
    ///
    /// Exec uses this to preserve the process lifetime mark while replacing
    /// the address space; fork uses the same operation for the child's
    /// inherited initial peak.
    pub(crate) fn merge_resident_highwater(&self, resident_kb: u64) -> u64 {
        let mut current = self.maxrss_kb.load(Ordering::Acquire);
        while resident_kb > current {
            match self.maxrss_kb.compare_exchange_weak(
                current,
                resident_kb,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return resident_kb,
                Err(observed) => current = observed,
            }
        }
        current
    }

    /// Publishes the current resident set before a mutation can remove PTEs.
    /// The caller already owns the address-space lock.
    pub(super) fn publish_resident_highwater(&self) {
        self.merge_resident_highwater(self.resident_user_bytes() as u64 / 1024);
    }

    pub fn lock_current_mappings(&mut self) {
        let ranges: Vec<_> = self
            .areas
            .iter()
            .map(|area| (area.start(), area.end()))
            .collect();
        for (start, end) in ranges {
            self.insert_locked_range(start, end);
        }
    }

    pub fn set_lock_future_mappings(&mut self, enabled: bool, on_fault: bool) {
        self.lock_future_mappings = enabled;
        self.lock_future_on_fault = enabled && on_fault;
    }

    pub fn locks_future_mappings(&self) -> bool {
        self.lock_future_mappings
    }

    pub fn locks_future_mappings_on_fault(&self) -> bool {
        self.lock_future_on_fault
    }

    pub fn clear_locked_mappings(&mut self) {
        self.locked_ranges.clear();
        let secret_ranges: Vec<_> = self
            .areas()
            .filter(|area| area.backend().is_secret())
            .map(|area| (area.start(), area.end()))
            .collect();
        for (start, end) in secret_ranges {
            self.insert_locked_range(start, end);
        }
        self.lock_future_mappings = false;
        self.lock_future_on_fault = false;
    }

    pub(super) fn validate_region(&self, start: VirtAddr, size: usize) -> AxResult {
        if !self.contains_range(start, size) {
            ax_bail!(NoMemory, "address out of range");
        }
        if !start.is_aligned_4k() || !is_aligned_4k(size) {
            ax_bail!(InvalidInput, "address is not aligned");
        }
        Ok(())
    }

    /// Finds a free area that can accommodate the given size.
    ///
    /// The search starts from the given hint address, and the area should be
    /// within the given limit range.
    ///
    /// Returns the start address of the free area. Returns None if no such area
    /// is found.
    pub fn find_free_area(
        &self,
        hint: VirtAddr,
        size: usize,
        limit: VirtAddrRange,
        align: usize,
    ) -> Option<VirtAddr> {
        self.areas.find_free_area(hint, size, limit, align)
    }

    /// Finds a free interval while treating the page immediately below every
    /// shadow-stack VMA as occupied.  Guard pages are policy, not VMAs, so
    /// every automatic allocator must use this helper rather than a one-shot
    /// caller-side retry.
    pub fn find_free_area_avoiding_shadow_stack_guards(
        &self,
        hint: VirtAddr,
        size: usize,
        limit: VirtAddrRange,
        align: usize,
    ) -> Option<VirtAddr> {
        let mut hint = hint;
        loop {
            let candidate = self.find_free_area(hint, size, limit, align)?;
            let end = candidate.checked_add(size)?;
            let retry = self
                .areas()
                .filter(|area| area.flags().contains(MappingFlags::SHADOW_STACK))
                .filter_map(|area| {
                    let guard_end = area.start();
                    let guard_start = guard_end.checked_sub(PAGE_SIZE_4K)?;
                    (candidate < guard_end && guard_start < end).then_some(guard_end)
                })
                .max();
            let Some(next) = retry else {
                return Some(candidate);
            };
            // All VMAs and guard boundaries are page-aligned; still defend
            // against malformed state so the retry cannot spin forever.
            if next <= hint || next <= candidate {
                return None;
            }
            hint = next;
        }
    }

    /// Finds a free area for kernel-chosen placement.
    ///
    /// If the caller provides an explicit hint above the base, that hint is
    /// still tried first. Otherwise, or if the explicit hint fails, the search
    /// first tries an append-biased placement near the current high-water mark
    /// before falling back to the full first-fit scan from the address-space
    /// base.
    pub fn find_kernel_area(
        &self,
        hint: VirtAddr,
        size: usize,
        limit: VirtAddrRange,
        align: usize,
    ) -> Option<VirtAddr> {
        if hint > limit.start {
            self.find_free_area_avoiding_shadow_stack_guards(hint, size, limit, align)
                .or_else(|| {
                    self.find_free_area_avoiding_shadow_stack_guards(
                        limit.start,
                        size,
                        limit,
                        align,
                    )
                })
        } else {
            self.areas.find_append_area(size, limit, align).or_else(|| {
                self.find_free_area_avoiding_shadow_stack_guards(limit.start, size, limit, align)
            })
        }
    }

    pub fn find_area(&self, vaddr: VirtAddr) -> Option<&MemoryArea<Backend>> {
        self.areas.find(vaddr)
    }
}
