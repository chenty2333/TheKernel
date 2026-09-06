/// Stable policy failures returned by `thekernel-linux-mm`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MmError {
    /// A range, limit, or capacity that must be nonzero was zero.
    ZeroLength,
    /// Address, size, page, or accounting arithmetic overflowed.
    Overflow,
    /// A range exceeds the consumer-supplied userspace upper bound.
    AddressOutOfRange,
    /// A page size was not a power of two.
    InvalidPageSize,
    /// A page operation received an unaligned address or length.
    Unaligned,
    /// A stable identity used the reserved zero value.
    InvalidIdentity,
    /// A requested range is outside the frozen mapping.
    RangeNotMapped,
    /// The frozen mapping does not permit the requested access.
    AccessDenied,
    /// Raw mapping-access bits contain an unknown flag.
    InvalidAccess,
    /// The mapping kind cannot support the requested pin lifetime or use.
    UnsupportedPin,
    /// Per-owner or global pin accounting would exceed its finite quota.
    QuotaExceeded,
    /// A system pin-budget charge was presented to a different budget.
    BudgetMismatch,
    /// A caller-owned bounded registry has no free slot.
    CapacityExceeded,
    /// A resource limit used the reserved effectively-unbounded maximum value.
    UnboundedLimit,
    /// No quota is configured for the requested owner.
    OwnerNotConfigured,
    /// A quota cannot be removed or replaced while it has live charges.
    OwnerBusy,
    /// A writable pin overlaps a live reservation or short-lived pin.
    PinOverlap,
    /// A mapping mutation overlaps a live pin.
    MappingPinned,
    /// A mapping identity or generation changed after the operation snapshot.
    StaleGeneration,
    /// A token does not name a live registry record.
    UnknownToken,
    /// A token exists but is in the wrong lifecycle state.
    InvalidTokenState,
    /// A non-wrapping identity sequence has been exhausted.
    IdExhausted,
    /// The registry has stopped admitting new work.
    Closing,
    /// Teardown has begun and the requested operation is no longer admitted.
    TearingDown,
    /// The registry is fully closed.
    Closed,
    /// Live work prevents the requested lifecycle transition.
    Busy,
    /// A fault ticket no longer names a pending request.
    UnknownFault,
    /// An exact lower-broker request snapshot does not match the fault being
    /// admitted.
    FaultRequestMismatch,
    /// Raw `userfaultfd(2)` creation flags contain an unknown bit.
    InvalidUffdFlags,
    /// The requested userfaultfd API number does not match Linux `UFFD_API`.
    InvalidUffdApi,
    /// A userfaultfd feature mask contains a bit outside the pinned Linux ABI.
    InvalidUffdFeatures,
    /// A known Linux userfaultfd feature is outside the bounded initial profile.
    UnsupportedUffdFeatures,
    /// `UFFDIO_API` has not initialized this userfaultfd policy context.
    UffdNotInitialized,
    /// Linux permits `UFFDIO_API` negotiation only once per context.
    UffdAlreadyInitialized,
    /// A userfaultfd registration mode is empty or contains an unknown bit.
    InvalidUffdMode,
    /// A known registration mode is outside the MISSING-only initial profile.
    UnsupportedUffdMode,
    /// A mapping kind is outside the anonymous-private initial profile.
    UnsupportedUffdMapping,
    /// A userfaultfd registration interval overlaps an existing owner.
    UffdRegistrationOverlap,
    /// A multi-VMA registration batch mixes owners or overlapping fragments.
    InvalidUffdRegistrationBatch,
    /// A registration token or exact registered interval is not present.
    UnknownUffdRegistration,
    /// A fault does not belong to the supplied registration interval or handler.
    UffdRegistrationMismatch,
    /// A fault disposition is outside the MISSING-only resolution profile.
    UnsupportedUffdDisposition,
    /// Raw UFFDIO_COPY mode contains a bit outside Linux v6.12.
    InvalidUffdCopyMode,
    /// A consumer rejects a known UFFDIO_COPY mode outside its bounded profile.
    UnsupportedUffdCopyMode,
    /// Raw UFFDIO_ZEROPAGE mode contains an unsupported bit.
    InvalidUffdZeroPageMode,
    /// COPY/ZEROPAGE progress is zero, non-page-aligned, or out of range.
    InvalidUffdProgress,
    /// A remap segment is outside the source prefix or otherwise inconsistent.
    InvalidRemap,
    /// Mapping segments supplied for pin revalidation are not contiguous.
    NonContiguousCoverage,
    /// Publication was attempted before all pin pages were revalidated.
    IncompleteRevalidation,
    /// Memlock is disabled for this caller.
    MemlockDenied,
    /// Caller-supplied current/covered accounting is internally inconsistent.
    InconsistentAccounting,
}
