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
