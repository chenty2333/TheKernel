use core::fmt;

use memory_addr::AddrRange;

use crate::{MappingBackend, MappingError, MappingLineage, MappingResult};

/// A memory area represents a continuous range of virtual memory with the same
/// flags.
///
/// The target physical memory frames are determined by [`MappingBackend`] and
/// may not be contiguous.
pub struct MemoryArea<B: MappingBackend> {
    va_range: AddrRange<B::Addr>,
    flags: B::Flags,
    backend: B,
    lineage: MappingLineage,
}

impl<B: MappingBackend> MemoryArea<B> {
    /// Creates a new memory area.
    ///
    /// This compatibility constructor uses [`MappingLineage::UNTRACKED`], so
    /// compatible adjacent areas retain the crate's historical merge behavior.
    /// Identity-aware consumers should use [`Self::new_with_lineage`].
    ///
    /// # Panics
    ///
    /// Panics if `start + size` overflows.
    pub fn new(start: B::Addr, size: usize, flags: B::Flags, backend: B) -> Self {
        Self::new_with_lineage(start, size, flags, backend, MappingLineage::UNTRACKED)
    }

    /// Creates a memory area with a caller-owned logical mapping lineage.
    ///
    /// # Panics
    ///
    /// Panics if `start + size` overflows.
    pub fn new_with_lineage(
        start: B::Addr,
        size: usize,
        flags: B::Flags,
        backend: B,
        lineage: MappingLineage,
    ) -> Self {
        Self {
            va_range: AddrRange::from_start_size(start, size),
            flags,
            backend,
            lineage,
        }
    }

    /// Returns the virtual address range.
    pub const fn va_range(&self) -> AddrRange<B::Addr> {
        self.va_range
    }

    /// Returns the memory flags, e.g., the permission bits.
    pub const fn flags(&self) -> B::Flags {
        self.flags
    }

    /// Returns the start address of the memory area.
    pub const fn start(&self) -> B::Addr {
        self.va_range.start
    }

    /// Returns the end address of the memory area.
    pub const fn end(&self) -> B::Addr {
        self.va_range.end
    }

    /// Returns the size of the memory area.
    pub fn size(&self) -> usize {
        self.va_range.size()
    }

    /// Returns the mapping backend of the memory area.
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    /// Mutates backend-owned VMA metadata without touching the page table.
    /// Callers must use a set-level transaction when the update can require
    /// VMA boundary splits.
    pub(crate) fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Returns the opaque lineage shared by fragments of this logical mapping.
    pub const fn lineage(&self) -> MappingLineage {
        self.lineage
    }
}

impl<B: MappingBackend> MemoryArea<B> {
    /// Changes the flags after the backend has committed the protection.
    pub(crate) fn set_flags(&mut self, new_flags: B::Flags) {
        self.flags = new_flags;
    }

    /// Changes the end address of the memory area.
    pub(crate) fn set_end(&mut self, new_end: B::Addr) {
        self.va_range.end = new_end;
    }

    /// Changes the start address of the memory area.
    pub(crate) fn set_start(&mut self, new_start: B::Addr) {
        self.va_range.start = new_start;
    }

    /// Maps the whole memory area in the page table.
    pub(crate) fn map_area(&self, page_table: &mut B::PageTable) -> MappingResult {
        self.backend
            .map(self.start(), self.size(), self.flags, page_table)
            .then_some(())
            .ok_or(MappingError::BadState)
    }
}

impl<B: MappingBackend> fmt::Debug for MemoryArea<B>
where
    B::Addr: fmt::Debug,
    B::Flags: fmt::Debug + Copy,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MemoryArea")
            .field("va_range", &self.va_range)
            .field("flags", &self.flags)
            .field("lineage", &self.lineage)
            .finish()
    }
}
