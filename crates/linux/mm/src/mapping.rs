use crate::{
    AddressSpaceId, MappingGeneration, MappingId, MmError, PageRange, PageSize, UserRange,
};

/// Linux-visible read, write, and execute permissions for a frozen mapping.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct MappingAccess(u8);

impl MappingAccess {
    /// Read permission bit.
    pub const READ: u8 = 1 << 0;
    /// Write permission bit.
    pub const WRITE: u8 = 1 << 1;
    /// Execute permission bit.
    pub const EXECUTE: u8 = 1 << 2;
    const VALID: u8 = Self::READ | Self::WRITE | Self::EXECUTE;

    /// Builds an access value from booleans.
    pub const fn new(read: bool, write: bool, execute: bool) -> Self {
        Self(
            (if read { Self::READ } else { 0 })
                | (if write { Self::WRITE } else { 0 })
                | (if execute { Self::EXECUTE } else { 0 }),
        )
    }

    /// Validates raw permission bits.
    pub const fn from_bits(bits: u8) -> Result<Self, MmError> {
        if bits & !Self::VALID != 0 {
            return Err(MmError::InvalidAccess);
        }
        Ok(Self(bits))
    }

    /// Raw stable bit representation.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether reads are permitted.
    pub const fn readable(self) -> bool {
        self.0 & Self::READ != 0
    }

    /// Whether writes are permitted.
    pub const fn writable(self) -> bool {
        self.0 & Self::WRITE != 0
    }

    /// Whether instruction fetch is permitted.
    pub const fn executable(self) -> bool {
        self.0 & Self::EXECUTE != 0
    }
}

/// Policy-relevant mapping class without a concrete VMA/backend type.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum MappingKind {
    /// Anonymous private mapping with COW semantics.
    AnonymousPrivate,
    /// Anonymous mapping shared by its participants.
    AnonymousShared,
    /// Private file mapping with COW semantics.
    FilePrivate,
    /// Shared file mapping.
    FileShared,
    /// Device-backed mapping.
    Device,
    /// Other consumer-defined special mapping not pinnable by default.
    Special,
}

/// Immutable mapping facts captured before blocking or fallible work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappingSnapshot {
    address_space: AddressSpaceId,
    mapping: MappingId,
    generation: MappingGeneration,
    range: PageRange,
    access: MappingAccess,
    kind: MappingKind,
    long_term_pinnable: bool,
    writable_file_pin_supported: bool,
}

impl MappingSnapshot {
    /// Builds a mapping snapshot from validated identities and range facts.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        address_space: AddressSpaceId,
        mapping: MappingId,
        generation: MappingGeneration,
        range: PageRange,
        access: MappingAccess,
        kind: MappingKind,
        long_term_pinnable: bool,
        writable_file_pin_supported: bool,
    ) -> Self {
        Self {
            address_space,
            mapping,
            generation,
            range,
            access,
            kind,
            long_term_pinnable,
            writable_file_pin_supported,
        }
    }

    /// Ergonomic raw-value adapter for a consumer snapshot boundary.
    #[allow(clippy::too_many_arguments)]
    pub fn from_raw(
        address_space: u64,
        mapping: u64,
        generation: u64,
        start: usize,
        length: usize,
        page_size: usize,
        access_bits: u8,
        kind: MappingKind,
        long_term_pinnable: bool,
        writable_file_pin_supported: bool,
    ) -> Result<Self, MmError> {
        Ok(Self::new(
            AddressSpaceId::new(address_space)?,
            MappingId::new(mapping)?,
            MappingGeneration::new(generation)?,
            PageRange::new(start, length, page_size)?,
            MappingAccess::from_bits(access_bits)?,
            kind,
            long_term_pinnable,
            writable_file_pin_supported,
        ))
    }

    /// Address-space identity.
    pub const fn address_space(self) -> AddressSpaceId {
        self.address_space
    }

    /// Logical mapping identity.
    pub const fn mapping(self) -> MappingId {
        self.mapping
    }

    /// Mapping generation.
    pub const fn generation(self) -> MappingGeneration {
        self.generation
    }

    /// Exact mapped page range.
    pub const fn range(self) -> PageRange {
        self.range
    }

    /// Frozen mapping access.
    pub const fn access(self) -> MappingAccess {
        self.access
    }

    /// Frozen mapping class.
    pub const fn kind(self) -> MappingKind {
        self.kind
    }

    /// Whether the consumer mechanism admits long-term pins.
    pub const fn long_term_pinnable(self) -> bool {
        self.long_term_pinnable
    }

    /// Whether writable shared-file pins have a complete dirty/writeback contract.
    pub const fn writable_file_pin_supported(self) -> bool {
        self.writable_file_pin_supported
    }

    /// Returns the same mechanism/policy facts with a caller-selected
    /// generation.
    ///
    /// A userfaultfd adapter can project a current VMA snapshot onto the
    /// registration/fault epoch before key revalidation without rebuilding or
    /// accidentally changing any other field.
    pub const fn with_generation(self, generation: MappingGeneration) -> Self {
        Self { generation, ..self }
    }

    /// Creates the immutable identity/generation expectation for later revalidation.
    pub const fn expected(self) -> ExpectedMapping {
        ExpectedMapping {
            address_space: self.address_space,
            mapping: self.mapping,
            generation: self.generation,
            range: self.range,
        }
    }
}

/// Exact mapping identity, generation, and range expected at publication.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExpectedMapping {
    address_space: AddressSpaceId,
    mapping: MappingId,
    generation: MappingGeneration,
    range: PageRange,
}

impl ExpectedMapping {
    /// Address-space identity.
    pub const fn address_space(self) -> AddressSpaceId {
        self.address_space
    }

    /// Mapping identity.
    pub const fn mapping(self) -> MappingId {
        self.mapping
    }

    /// Expected generation.
    pub const fn generation(self) -> MappingGeneration {
        self.generation
    }

    /// Expected mapped range.
    pub const fn range(self) -> PageRange {
        self.range
    }

    /// Rejects replacement, generation change, shrink, or page-size change.
    pub fn revalidate(self, current: MappingSnapshot) -> Result<(), MmError> {
        if self.address_space != current.address_space
            || self.mapping != current.mapping
            || self.generation != current.generation
        {
            return Err(MmError::StaleGeneration);
        }
        if !current.range.contains(self.range) {
            return Err(MmError::RangeNotMapped);
        }
        Ok(())
    }

    /// Revalidates identity/generation and an operation's exact covered range.
    pub fn revalidate_range(
        self,
        current: MappingSnapshot,
        operation_range: PageRange,
    ) -> Result<(), MmError> {
        self.revalidate(current)?;
        if !self.range.contains(operation_range) || !current.range.contains(operation_range) {
            return Err(MmError::RangeNotMapped);
        }
        Ok(())
    }
}

/// Why an old mapping generation is being invalidated.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum InvalidationReason {
    /// Access protections are changing.
    Protect,
    /// Pages are being unmapped.
    Unmap,
    /// Mapping topology or virtual location is changing.
    Remap,
    /// A private mapping is breaking COW.
    CowBreak,
    /// Fork changes sharing/generation state.
    Fork,
    /// File truncate invalidates the covered range.
    Truncate,
    /// Resident pages are discarded while the VMA topology remains.
    Discard,
    /// Hole punch invalidates file-backed pages.
    HolePunch,
    /// Filesystem or page-cache invalidation.
    FileInvalidation,
    /// Exec replaces the address space.
    Exec,
    /// Address-space teardown.
    Teardown,
}

/// Typed old-generation invalidation published around a mapping mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidationRange {
    expected: ExpectedMapping,
    range: PageRange,
    reason: InvalidationReason,
}

impl InvalidationRange {
    /// Builds an invalidation contained by the frozen old mapping.
    pub const fn new(
        snapshot: MappingSnapshot,
        range: PageRange,
        reason: InvalidationReason,
    ) -> Result<Self, MmError> {
        if !snapshot.range.contains(range) {
            return Err(MmError::RangeNotMapped);
        }
        Ok(Self {
            expected: snapshot.expected(),
            range,
            reason,
        })
    }

    /// Raw-value adapter for a byte range using the snapshot page size.
    pub fn from_raw(
        snapshot: MappingSnapshot,
        start: usize,
        length: usize,
        reason: InvalidationReason,
    ) -> Result<Self, MmError> {
        let page_size = snapshot.range().page_size().bytes();
        Self::new(snapshot, PageRange::new(start, length, page_size)?, reason)
    }

    /// Frozen mapping expectation.
    pub const fn expected(self) -> ExpectedMapping {
        self.expected
    }

    /// Exact invalidated range.
    pub const fn range(self) -> PageRange {
        self.range
    }

    /// Invalidation reason.
    pub const fn reason(self) -> InvalidationReason {
        self.reason
    }

    /// Builds an invalidation covering one arbitrary nonempty byte range.
    pub fn covering_bytes(
        snapshot: MappingSnapshot,
        range: UserRange,
        reason: InvalidationReason,
    ) -> Result<Self, MmError> {
        let page_size = PageSize::new(snapshot.range().page_size().bytes())?;
        Self::new(snapshot, PageRange::covering(range, page_size)?, reason)
    }
}
