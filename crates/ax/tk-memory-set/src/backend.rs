use memory_addr::MemoryAddr;

/// Underlying operations to do when manipulating mappings within the specific
/// [`MemoryArea`](crate::MemoryArea).
///
/// The backend can be different for different memory areas. e.g., for linear
/// mappings, the target physical address is known when it is added to the page
/// table. For lazy mappings, an empty mapping needs to be added to the page
/// table to trigger a page fault.
pub trait MappingBackend: Clone {
    /// The address type used in the memory area.
    type Addr: MemoryAddr;
    /// The flags type used in the memory area.
    type Flags: Copy + PartialEq;
    /// The page table type used in the memory area.
    type PageTable;

    /// What to do when mapping a region within the area with the given flags.
    fn map(
        &self,
        start: Self::Addr,
        size: usize,
        flags: Self::Flags,
        page_table: &mut Self::PageTable,
    ) -> bool;

    /// Validates installing a mapping without changing mapping metadata or
    /// the page table.
    ///
    /// Callers that do not need to retain an allocation-bearing map plan may
    /// use this as a read-only admission check.
    /// After this method has returned `true` under stable page-table and VMA
    /// serialization, [`Self::map`] must not report a recoverable failure.
    /// Returning `false` from `map` after successful admission is therefore
    /// an internal consistency failure, just as it is after
    /// [`Self::preflight_unmap`] or [`Self::preflight_protect`].
    ///
    /// The default rejects this stronger admission. Existing callers of
    /// [`Self::map`] remain source-compatible, while a backend must opt in
    /// explicitly before a fixed-replacement transaction may withdraw old
    /// PTEs on the strength of this check.
    fn preflight_map(
        &self,
        start: Self::Addr,
        size: usize,
        flags: Self::Flags,
        page_table: &Self::PageTable,
    ) -> bool {
        let _ = (start, size, flags, page_table);
        false
    }

    /// Validates an unmap without changing either mapping metadata or the page
    /// table.
    ///
    /// [`MemorySet::unmap`](crate::MemorySet::unmap) runs every overlapping
    /// backend preflight before its first mutation. Once this returns `true`,
    /// [`Self::unmap`] must not report a recoverable failure unless the page
    /// table is changed independently between the two calls. Violating that
    /// contract is an internal consistency failure and the caller may abort
    /// rather than return with a partially removed mapping.
    ///
    /// The default accepts the operation and therefore keeps legacy backends
    /// on that fail-stop contract. Backends with fallible structural checks
    /// should override this method so those failures remain recoverable.
    fn preflight_unmap(
        &self,
        start: Self::Addr,
        size: usize,
        page_table: &Self::PageTable,
    ) -> bool {
        let _ = (start, size, page_table);
        true
    }

    /// What to do when unmaping a memory region within the area.
    fn unmap(&self, start: Self::Addr, size: usize, page_table: &mut Self::PageTable) -> bool;

    /// What to do when changing access flags.
    ///
    /// This read-only admission hook is called for every affected backend
    /// before [`MemorySet::protect`](crate::MemorySet::protect) changes either
    /// the area tree or the page table. Once it returns `true`, [`Self::protect`]
    /// must not report a recoverable failure unless the page table is changed
    /// independently between the two calls. Violating that contract is an
    /// internal consistency failure and the caller may abort rather than leave
    /// VMA metadata and PTE permissions out of sync.
    ///
    /// The default accepts the operation and therefore keeps legacy backends
    /// on that fail-stop contract. Backends with fallible structural or policy
    /// checks should override this method.
    fn preflight_protect(
        &self,
        start: Self::Addr,
        size: usize,
        new_flags: Self::Flags,
        page_table: &Self::PageTable,
    ) -> bool {
        let _ = (start, size, new_flags, page_table);
        true
    }

    /// Commits a previously admitted access-flag change.
    fn protect(
        &self,
        start: Self::Addr,
        size: usize,
        new_flags: Self::Flags,
        page_table: &mut Self::PageTable,
    ) -> bool;

    /// Returns whether this backend can be coalesced with an adjacent backend
    /// that has the same mapping flags.
    fn can_merge(&self, _other: &Self) -> bool {
        false
    }
}

/// Optional extension for backends that must defer resource retirement until
/// an external translation fence has completed.
///
/// [`MemorySet::unmap_deferred`](crate::MemorySet::unmap_deferred) and
/// [`MemorySet::clear_deferred`](crate::MemorySet::clear_deferred) retain every
/// returned [`Self::Retirement`] together with any fully removed
/// [`MemoryArea`](crate::MemoryArea). The caller releases both explicitly after
/// completing its architecture-specific fence. The original [`MappingBackend`]
/// API remains available for callers whose unmap resources may be retired
/// immediately.
pub trait DeferredUnmapBackend: MappingBackend {
    /// Backend-owned state that must survive until the caller's fence.
    type Retirement;

    /// Commits a previously admitted unmap and transfers its retirement state.
    ///
    /// This method follows the same fail-stop contract as
    /// [`MappingBackend::unmap`]: after [`MappingBackend::preflight_unmap`]
    /// succeeds under stable page-table and topology serialization, returning
    /// [`None`] is an internal consistency failure.
    fn unmap_deferred(
        &self,
        start: Self::Addr,
        size: usize,
        page_table: &mut Self::PageTable,
    ) -> Option<Self::Retirement>;

    /// Allocates and admits an exact deferred-unmap snapshot plan.
    ///
    /// Fixed replacement invokes this during its private preparation phase,
    /// before withdrawing any PTE. The returned token must contain all
    /// capacity and page-table-side state needed by
    /// [`Self::unmap_deferred_prepared`]; that later commit hook must neither
    /// allocate nor report a recoverable failure under stable serialization.
    /// It may initially represent an empty reservation and become the actual
    /// retirement token only when the old leaves are withdrawn.
    ///
    /// The rejecting default keeps ordinary deferred-unmap backends source
    /// compatible while making fixed replacement an explicit opt-in.
    fn prepare_deferred_unmap(
        &self,
        start: Self::Addr,
        size: usize,
        page_table: &Self::PageTable,
    ) -> Option<Self::Retirement> {
        let _ = (start, size, page_table);
        None
    }

    /// Commits a deferred unmap using a token returned by
    /// [`Self::prepare_deferred_unmap`].
    ///
    /// On success `retirement` contains the exact withdrawn leaf state and
    /// remains suitable for [`Self::restore_deferred`]. Returning `false`
    /// after a successful preparation is an internal consistency failure.
    fn unmap_deferred_prepared(
        &self,
        retirement: &mut Self::Retirement,
        start: Self::Addr,
        size: usize,
        page_table: &mut Self::PageTable,
    ) -> bool {
        let _ = (retirement, start, size, page_table);
        false
    }

    /// Allocates and admits all page-table-side resources for the incoming
    /// side of a fixed replacement.
    ///
    /// The opaque value uses [`Self::Retirement`] so existing deferred-unmap
    /// implementations remain source-compatible. An implementation may use a
    /// tagged token internally to distinguish incoming-map reservations from
    /// withdrawn-leaf snapshots. The reservation is made while the complete
    /// replacement is still private; [`Self::map_fixed_prepared`] must install
    /// it without allocation or recoverable failure.
    fn prepare_fixed_map(
        &self,
        start: Self::Addr,
        size: usize,
        flags: Self::Flags,
        page_table: &Self::PageTable,
    ) -> Option<Self::Retirement> {
        let _ = (start, size, flags, page_table);
        None
    }

    /// Installs an incoming fixed-replacement mapping from a token returned
    /// by [`Self::prepare_fixed_map`].
    ///
    /// The token is consumed. On success its destructor must not release the
    /// newly installed mapping or prepared intermediate page-table frames. On
    /// a post-admission failure it must retain resources for fail-stop cleanup.
    fn map_fixed_prepared(
        &self,
        preparation: Self::Retirement,
        start: Self::Addr,
        size: usize,
        flags: Self::Flags,
        page_table: &mut Self::PageTable,
    ) -> bool {
        let _ = (preparation, start, size, flags, page_table);
        false
    }

    /// Validates restoring the exact PTE snapshot carried by one future
    /// deferred-unmap retirement token.
    ///
    /// A fixed replacement calls this before it withdraws the old mapping.
    /// An implementation that supports rollback must guarantee that a later
    /// [`Self::restore_deferred`] succeeds after this admission while the
    /// caller keeps page-table and VMA topology serialized.
    fn preflight_restore_deferred(
        &self,
        start: Self::Addr,
        size: usize,
        page_table: &Self::PageTable,
    ) -> bool {
        let _ = (start, size, page_table);
        false
    }

    /// Restores the exact leaf PTE state represented by `retirement`.
    ///
    /// This is deliberately not expressed as a fresh [`MappingBackend::map`]
    /// call: COW and shared mappings may have faulted, split, or otherwise
    /// changed resident PTE state after their VMA was created. A backend that
    /// opts into prepared fixed replacement must preserve that state in its
    /// deferred retirement token and restore it here without allocation.
    ///
    /// The token is consumed even on failure. On success its destructor must
    /// not retire the newly restored leaves; on failure the backend must keep
    /// any still-live snapshot resources until fail-stop handling takes over.
    ///
    /// The default rejects restoration, leaving existing deferred-unmap
    /// backends source-compatible while making the stronger fixed-replacement
    /// primitive unavailable until they implement an exact snapshot path.
    fn restore_deferred(
        &self,
        retirement: Self::Retirement,
        start: Self::Addr,
        size: usize,
        page_table: &mut Self::PageTable,
    ) -> bool {
        let _ = (retirement, start, size, page_table);
        false
    }
}
