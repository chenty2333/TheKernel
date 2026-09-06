use core::num::NonZeroU64;

use crate::{
    AddressSpaceId, FaultAdmission, FaultAdmissionContext, FaultAdmissionPermit,
    FaultCompletionPermit, FaultDisposition, FaultHandlerId, FaultLifecycleState, FaultRequest,
    MappingGeneration, MappingId, MappingKind, MappingSnapshot, MmError, PageRange, UserRange,
    validate_fault_completion,
};

/// Linux v6.12 userfaultfd API version.
pub const UFFD_API: u64 = 0xaa;
/// Linux userfaultfd(2) flag restricting interception to user-mode faults.
pub const UFFD_USER_MODE_ONLY: u32 = 1;
/// Linux O_NONBLOCK value accepted by userfaultfd(2).
pub const UFFD_O_NONBLOCK: u32 = 0x800;
/// Linux O_CLOEXEC value accepted by userfaultfd(2).
pub const UFFD_O_CLOEXEC: u32 = 0x8_0000;

const UFFD_CREATE_VALID_FLAGS: u32 = UFFD_USER_MODE_ONLY | UFFD_O_NONBLOCK | UFFD_O_CLOEXEC;

/// Checked creation flags for the Linux v6.12 userfaultfd(2) entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdCreateFlags(u32);

impl UffdCreateFlags {
    /// Rejects every bit outside O_CLOEXEC, O_NONBLOCK, and
    /// UFFD_USER_MODE_ONLY.
    ///
    /// This is the Linux flag-namespace check only. Call
    /// [Self::validate_profile] for the initial unprivileged profile gate.
    pub const fn from_bits(bits: u32) -> Result<Self, MmError> {
        if bits & !UFFD_CREATE_VALID_FLAGS != 0 {
            return Err(MmError::InvalidUffdFlags);
        }
        Ok(Self(bits))
    }

    /// Requires the bounded first profile's UFFD_USER_MODE_ONLY flag.
    ///
    /// [MmError::AccessDenied] is intentionally distinct from
    /// [MmError::InvalidUffdFlags], allowing the syscall adapter to map a
    /// missing user-mode-only gate to EPERM and unknown bits to EINVAL.
    pub const fn validate_profile(self) -> Result<Self, MmError> {
        if !self.user_mode_only() {
            return Err(MmError::AccessDenied);
        }
        Ok(self)
    }

    /// Raw Linux flag bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether the resulting FD must be close-on-exec.
    pub const fn close_on_exec(self) -> bool {
        self.0 & UFFD_O_CLOEXEC != 0
    }

    /// Whether reads from the resulting FD are nonblocking.
    pub const fn nonblocking(self) -> bool {
        self.0 & UFFD_O_NONBLOCK != 0
    }

    /// Whether only user-mode page faults may be intercepted.
    pub const fn user_mode_only(self) -> bool {
        self.0 & UFFD_USER_MODE_ONLY != 0
    }
}

/// Checked Linux v6.12 userfaultfd feature bits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UffdFeatures(u64);

impl UffdFeatures {
    pub const PAGEFAULT_FLAG_WP: Self = Self(1 << 0);
    pub const EVENT_FORK: Self = Self(1 << 1);
    pub const EVENT_REMAP: Self = Self(1 << 2);
    pub const EVENT_REMOVE: Self = Self(1 << 3);
    pub const MISSING_HUGETLBFS: Self = Self(1 << 4);
    pub const MISSING_SHMEM: Self = Self(1 << 5);
    pub const EVENT_UNMAP: Self = Self(1 << 6);
    pub const SIGBUS: Self = Self(1 << 7);
    pub const THREAD_ID: Self = Self(1 << 8);
    pub const MINOR_HUGETLBFS: Self = Self(1 << 9);
    pub const MINOR_SHMEM: Self = Self(1 << 10);
    pub const EXACT_ADDRESS: Self = Self(1 << 11);
    pub const WP_HUGETLBFS_SHMEM: Self = Self(1 << 12);
    pub const WP_UNPOPULATED: Self = Self(1 << 13);
    pub const POISON: Self = Self(1 << 14);
    pub const WP_ASYNC: Self = Self(1 << 15);
    pub const MOVE: Self = Self(1 << 16);

    /// Every feature bit defined by the pinned Linux v6.12 UAPI.
    pub const LINUX_V6_12: Self = Self((1 << 17) - 1);
    /// The bounded first profile advertises no optional feature.
    ///
    /// Missing faults and the WRITE page-fault flag are implicit in the base
    /// API. WP, MINOR, thread-ID, exact-byte-address, shmem/hugetlb, SIGBUS,
    /// poison, move, and lifecycle events require later adapter proof.
    pub const AVAILABLE: Self = Self(0);

    /// Validates that all bits exist in Linux v6.12.
    pub const fn from_bits(bits: u64) -> Result<Self, MmError> {
        if bits & !Self::LINUX_V6_12.0 != 0 {
            return Err(MmError::InvalidUffdFeatures);
        }
        Ok(Self(bits))
    }

    /// Raw Linux feature bits.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether every feature in other is present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Userfaultfd ioctl command-number bitmask returned by negotiation/register.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UffdIoctls(u64);

impl UffdIoctls {
    pub const REGISTER: Self = Self(1 << 0);
    pub const UNREGISTER: Self = Self(1 << 1);
    pub const WAKE: Self = Self(1 << 2);
    pub const COPY: Self = Self(1 << 3);
    pub const ZEROPAGE: Self = Self(1 << 4);
    pub const MOVE: Self = Self(1 << 5);
    pub const WRITEPROTECT: Self = Self(1 << 6);
    pub const CONTINUE: Self = Self(1 << 7);
    pub const POISON: Self = Self(1 << 8);
    pub const API: Self = Self(1 << 63);

    /// Context ioctls advertised by the bounded Linux v6.12 profile.
    pub const API_PROFILE: Self = Self(Self::REGISTER.0 | Self::UNREGISTER.0 | Self::API.0);
    /// Range ioctls advertised after a MISSING-only registration.
    pub const MISSING_RANGE_PROFILE: Self = Self(Self::WAKE.0 | Self::COPY.0 | Self::ZEROPAGE.0);

    /// Raw Linux ioctl-number mask.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether every ioctl in other is present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Input copied from Linux struct uffdio_api before negotiation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdApiRequest {
    api: u64,
    features: UffdFeatures,
}

impl UffdApiRequest {
    /// Validates the raw feature namespace while retaining the requested API.
    pub const fn from_raw(api: u64, features: u64) -> Result<Self, MmError> {
        let features = match UffdFeatures::from_bits(features) {
            Ok(features) => features,
            Err(error) => return Err(error),
        };
        Ok(Self { api, features })
    }

    /// Requested API number.
    pub const fn api(self) -> u64 {
        self.api
    }

    /// Requested optional features.
    pub const fn features(self) -> UffdFeatures {
        self.features
    }
}

/// Successful Linux UFFDIO_API output for this bounded profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdApiResponse {
    api: u64,
    features: UffdFeatures,
    ioctls: UffdIoctls,
}

impl UffdApiResponse {
    /// Negotiated API number.
    pub const fn api(self) -> u64 {
        self.api
    }

    /// All optional features available from this profile, not merely enabled.
    pub const fn features(self) -> UffdFeatures {
        self.features
    }

    /// Context ioctl mask for Linux v6.12.
    pub const fn ioctls(self) -> UffdIoctls {
        self.ioctls
    }
}

/// Prepared API negotiation that is committed only after successful copyout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "copy out the response, then commit negotiation"]
pub struct UffdApiNegotiation {
    requested_features: UffdFeatures,
    response: UffdApiResponse,
}

impl UffdApiNegotiation {
    /// Successful response that must be copied to userspace before commit.
    pub const fn response(self) -> UffdApiResponse {
        self.response
    }

    /// Optional features that become enabled on commit.
    pub const fn requested_features(self) -> UffdFeatures {
        self.requested_features
    }
}

/// One-shot UFFDIO_API negotiation state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UffdApiLifecycle {
    Created,
    Initialized,
}

/// Pure one-shot API negotiation policy; the FD and usercopy stay in the adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UffdApiState {
    lifecycle: UffdApiLifecycle,
    enabled_features: UffdFeatures,
}

impl UffdApiState {
    pub const fn new() -> Self {
        Self {
            lifecycle: UffdApiLifecycle::Created,
            enabled_features: UffdFeatures(0),
        }
    }

    /// Validates Linux API 0xaa without changing initialization state.
    ///
    /// The adapter copies [UffdApiNegotiation::response] to userspace and
    /// calls [Self::commit] only after copyout succeeds. On any validation or
    /// commit error it must clear the complete userspace uffdio_api value,
    /// matching Linux v6.12.
    pub fn prepare(&self, request: UffdApiRequest) -> Result<UffdApiNegotiation, MmError> {
        if request.api != UFFD_API {
            return Err(MmError::InvalidUffdApi);
        }
        if request.features.bits() & !UffdFeatures::AVAILABLE.bits() != 0 {
            return Err(MmError::UnsupportedUffdFeatures);
        }
        if self.lifecycle != UffdApiLifecycle::Created {
            return Err(MmError::UffdAlreadyInitialized);
        }
        Ok(UffdApiNegotiation {
            requested_features: request.features,
            response: UffdApiResponse {
                api: UFFD_API,
                features: UffdFeatures::AVAILABLE,
                ioctls: UffdIoctls::API_PROFILE,
            },
        })
    }

    /// Convenience raw-value preparation entry for a syscall adapter.
    pub fn prepare_raw(&self, api: u64, features: u64) -> Result<UffdApiNegotiation, MmError> {
        self.prepare(UffdApiRequest::from_raw(api, features)?)
    }

    /// Commits initialization after the prepared response reached userspace.
    pub fn commit(&mut self, negotiation: UffdApiNegotiation) -> Result<UffdApiResponse, MmError> {
        if self.lifecycle != UffdApiLifecycle::Created {
            return Err(MmError::UffdAlreadyInitialized);
        }
        self.lifecycle = UffdApiLifecycle::Initialized;
        self.enabled_features = negotiation.requested_features;
        Ok(negotiation.response)
    }

    pub const fn lifecycle(&self) -> UffdApiLifecycle {
        self.lifecycle
    }

    /// Features actually requested and enabled for this context.
    pub const fn enabled_features(&self) -> UffdFeatures {
        self.enabled_features
    }

    fn require_initialized(&self) -> Result<(), MmError> {
        if self.lifecycle == UffdApiLifecycle::Initialized {
            Ok(())
        } else {
            Err(MmError::UffdNotInitialized)
        }
    }
}

impl Default for UffdApiState {
    fn default() -> Self {
        Self::new()
    }
}

/// Checked UFFDIO_REGISTER mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdRegisterMode(u64);

impl UffdRegisterMode {
    pub const MISSING: Self = Self(1 << 0);
    pub const WP: Self = Self(1 << 1);
    pub const MINOR: Self = Self(1 << 2);
    const LINUX_V6_12: u64 = Self::MISSING.0 | Self::WP.0 | Self::MINOR.0;

    /// Accepts exactly MISSING. WP and MINOR are recognized but unsupported.
    pub const fn from_bits(bits: u64) -> Result<Self, MmError> {
        if bits == 0 || bits & !Self::LINUX_V6_12 != 0 {
            return Err(MmError::InvalidUffdMode);
        }
        if bits != Self::MISSING.0 {
            return Err(MmError::UnsupportedUffdMode);
        }
        Ok(Self(bits))
    }

    /// Raw Linux registration-mode bits.
    pub const fn bits(self) -> u64 {
        self.0
    }
}

/// Nonzero, non-reused identity of one registered interval.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UffdRegistrationId(NonZeroU64);

impl UffdRegistrationId {
    pub const fn new(raw: u64) -> Result<Self, MmError> {
        match NonZeroU64::new(raw) {
            Some(raw) => Ok(Self(raw)),
            None => Err(MmError::InvalidIdentity),
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Validated request to own one anonymous-private MISSING interval.
///
/// The mapping generation is the adapter-supplied registration/fault epoch.
/// Refreshing fragments after `mprotect` or a topology-only split preserves
/// this epoch; replacing the logical mapping must supply new authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdRegistrationRequest {
    handler: FaultHandlerId,
    address_space: AddressSpaceId,
    mapping: MappingId,
    generation: MappingGeneration,
    range: PageRange,
    mode: UffdRegisterMode,
}

impl UffdRegistrationRequest {
    /// Freezes one mapped fragment of a possibly multi-VMA registration range.
    pub fn new(
        handler: FaultHandlerId,
        mapping: MappingSnapshot,
        range: PageRange,
        mode: UffdRegisterMode,
    ) -> Result<Self, MmError> {
        if mapping.kind() != MappingKind::AnonymousPrivate {
            return Err(MmError::UnsupportedUffdMapping);
        }
        if !mapping.range().contains(range) {
            return Err(MmError::RangeNotMapped);
        }
        if mode != UffdRegisterMode::MISSING {
            return Err(MmError::UnsupportedUffdMode);
        }
        Ok(Self {
            handler,
            address_space: mapping.address_space(),
            mapping: mapping.mapping(),
            generation: mapping.generation(),
            range,
            mode,
        })
    }

    pub const fn handler(self) -> FaultHandlerId {
        self.handler
    }

    pub const fn address_space(self) -> AddressSpaceId {
        self.address_space
    }

    pub const fn mapping(self) -> MappingId {
        self.mapping
    }

    pub const fn generation(self) -> MappingGeneration {
        self.generation
    }

    pub const fn range(self) -> PageRange {
        self.range
    }

    pub const fn mode(self) -> UffdRegisterMode {
        self.mode
    }
}

/// Published MISSING registration and the ioctl mask returned to userspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdRegistration {
    id: UffdRegistrationId,
    request: UffdRegistrationRequest,
}

impl UffdRegistration {
    pub const fn id(self) -> UffdRegistrationId {
        self.id
    }

    pub const fn handler(self) -> FaultHandlerId {
        self.request.handler
    }

    pub const fn address_space(self) -> AddressSpaceId {
        self.request.address_space
    }

    pub const fn mapping(self) -> MappingId {
        self.request.mapping
    }

    pub const fn generation(self) -> MappingGeneration {
        self.request.generation
    }

    pub const fn range(self) -> PageRange {
        self.request.range
    }

    pub const fn mode(self) -> UffdRegisterMode {
        self.request.mode
    }

    /// Per-range ioctls advertised for this MISSING-only interval.
    pub const fn ioctls(self) -> UffdIoctls {
        UffdIoctls::MISSING_RANGE_PROFILE
    }

    /// Builds the source-bound replacement for a preflighted in-place tail grow.
    ///
    /// Unlike [`Self::refreshed_fragment`], this helper does not require a
    /// post-grow [`MappingSnapshot`]. It accepts only the identity that the
    /// adapter froze before its MM transaction and a strictly larger range
    /// with the same start and page geometry. The replacement preserves this
    /// registration's handler, address space, mapping, registration/fault
    /// epoch, and mode, and can therefore be passed directly to
    /// [`UffdRegistrationTable::preflight_mapping_replace`].
    ///
    /// This method does not prove that the source registration reaches the old
    /// mapping end, nor that the mapping grow commits. The adapter owns both
    /// proofs and must not publish the replacement unless its MM transaction
    /// succeeds.
    pub fn tail_extension_replacement(
        self,
        address_space: AddressSpaceId,
        mapping: MappingId,
        new_range: PageRange,
    ) -> Result<UffdRegistrationReplacement, MmError> {
        if self.address_space() != address_space || self.mapping() != mapping {
            return Err(MmError::StaleGeneration);
        }
        let old_range = self.range();
        if old_range.page_size() != new_range.page_size()
            || new_range.start() != old_range.start()
            || !new_range.contains(old_range)
            || new_range.end() <= old_range.end()
        {
            return Err(MmError::RangeNotMapped);
        }
        Ok(UffdRegistrationReplacement::new(
            self.id(),
            UffdRegistrationRequest {
                range: new_range,
                ..self.request
            },
        ))
    }

    /// Builds the source-bound replacement for a preflighted grow-down head
    /// extension.
    ///
    /// This is the lower-bound counterpart of
    /// [`Self::tail_extension_replacement`]. The adapter supplies the frozen
    /// address-space/mapping identity and a strictly larger same-end range.
    /// The replacement keeps the source handler, logical mapping,
    /// registration/fault epoch, and mode without requiring a post-growth VMA
    /// snapshot.
    ///
    /// The adapter must prove that the source registration begins at the old
    /// mapping boundary and must commit this replacement only after the
    /// concrete grow-down mapping transaction succeeds.
    pub fn head_extension_replacement(
        self,
        address_space: AddressSpaceId,
        mapping: MappingId,
        new_range: PageRange,
    ) -> Result<UffdRegistrationReplacement, MmError> {
        if self.address_space() != address_space || self.mapping() != mapping {
            return Err(MmError::StaleGeneration);
        }
        let old_range = self.range();
        if old_range.page_size() != new_range.page_size()
            || new_range.end() != old_range.end()
            || !new_range.contains(old_range)
            || new_range.start() >= old_range.start()
        {
            return Err(MmError::RangeNotMapped);
        }
        Ok(UffdRegistrationReplacement::new(
            self.id(),
            UffdRegistrationRequest {
                range: new_range,
                ..self.request
            },
        ))
    }

    /// Builds one split/trim/grow refresh fragment without adopting a topology
    /// snapshot's generation or access.
    ///
    /// The fragment must be covered by the current anonymous-private mapping
    /// snapshot and retain the source registration's page geometry. The
    /// address-space and logical mapping identities must still match. The new
    /// range may extend beyond the old interval for an in-place mapping grow.
    /// On success the request preserves this registration's handler, mapping,
    /// registration/fault epoch, and mode.
    pub fn refreshed_fragment(
        self,
        current: MappingSnapshot,
        range: PageRange,
    ) -> Result<UffdRegistrationRequest, MmError> {
        if self.address_space() != current.address_space() || self.mapping() != current.mapping() {
            return Err(MmError::StaleGeneration);
        }
        if current.kind() != MappingKind::AnonymousPrivate {
            return Err(MmError::UnsupportedUffdMapping);
        }
        if self.range().page_size() != range.page_size()
            || current.range().page_size() != range.page_size()
            || !current.range().contains(range)
        {
            return Err(MmError::RangeNotMapped);
        }
        Ok(UffdRegistrationRequest {
            range,
            ..self.request
        })
    }

    /// Revalidates the mapping before the adapter publishes or resolves a fault.
    pub fn revalidate(self, current: MappingSnapshot) -> Result<(), MmError> {
        if self.address_space() != current.address_space()
            || self.mapping() != current.mapping()
            || self.generation() != current.generation()
        {
            return Err(MmError::StaleGeneration);
        }
        if current.kind() != MappingKind::AnonymousPrivate {
            return Err(MmError::UnsupportedUffdMapping);
        }
        if !current.range().contains(self.range()) {
            return Err(MmError::RangeNotMapped);
        }
        Ok(())
    }

    /// Proves at admission that a delegated MISSING request belongs to this
    /// exact interval and that its recorded access is currently permitted.
    pub fn validate_fault_admission(
        self,
        current: MappingSnapshot,
        request: FaultRequest,
    ) -> Result<(), MmError> {
        self.revalidate(current)?;
        if request.handler() != self.handler()
            || request.fault_type() != crate::FaultType::Missing
            || request.key().address_space() != self.address_space()
            || request.key().mapping() != self.mapping()
            || request.key().generation() != self.generation()
            || !self
                .range()
                .user_range()
                .contains_address(request.key().page_address().get())
        {
            return Err(MmError::UffdRegistrationMismatch);
        }
        request.key().revalidate_admission(current)
    }
}

/// One replacement fragment tied to the registration whose owner it inherits.
///
/// Mapping-level mutation may touch several userfaultfd handlers in one
/// address space. Pairing each new fragment with its source token lets the
/// table validate mixed-owner split/trim/grow refreshes without a giant
/// cross-subsystem trait or temporary ownership map.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdRegistrationReplacement {
    source: UffdRegistrationId,
    request: UffdRegistrationRequest,
}

impl UffdRegistrationReplacement {
    pub const fn new(source: UffdRegistrationId, request: UffdRegistrationRequest) -> Self {
        Self { source, request }
    }

    pub const fn source(self) -> UffdRegistrationId {
        self.source
    }

    pub const fn request(self) -> UffdRegistrationRequest {
        self.request
    }

    /// Canonicalizes two ordered replacement fragments inside one post-state
    /// anonymous-private VMA.
    ///
    /// This is a pure mapping-hook policy primitive: it neither allocates nor
    /// mutates a registration table. The post-state snapshot proves that both
    /// fragments remain in one logical mapping/VMA after the concrete MM
    /// transaction. Its topology generation and access bits are deliberately
    /// not adopted; the replacement keeps the registration/fault epoch and
    /// owner frozen in the source requests.
    ///
    /// Incompatible handler, fault epoch, mode, or a gap returns `Ok(None)`.
    /// Overlap or reversed input is an invalid adapter batch rather than a
    /// merge opportunity. On success `self.source` remains the representative
    /// source, but the caller must still remove `next.source` in the same
    /// all-or-none mapping-replacement transaction.
    pub fn canonical_union(
        self,
        next: Self,
        post_vma: MappingSnapshot,
    ) -> Result<Option<Self>, MmError> {
        if post_vma.kind() != MappingKind::AnonymousPrivate {
            return Err(MmError::UnsupportedUffdMapping);
        }

        let left = self.request;
        let right = next.request;
        if left.address_space != post_vma.address_space()
            || right.address_space != post_vma.address_space()
            || left.mapping != post_vma.mapping()
            || right.mapping != post_vma.mapping()
        {
            return Err(MmError::StaleGeneration);
        }
        if left.range.page_size() != post_vma.range().page_size()
            || right.range.page_size() != post_vma.range().page_size()
            || !post_vma.range().contains(left.range)
            || !post_vma.range().contains(right.range)
        {
            return Err(MmError::RangeNotMapped);
        }

        if left.range.end() > right.range.start() {
            return Err(MmError::InvalidUffdRegistrationBatch);
        }
        if left.handler != right.handler
            || left.generation != right.generation
            || left.mode != right.mode
            || left.range.end() < right.range.start()
        {
            return Ok(None);
        }

        let range = PageRange::with_page_size(
            left.range.start(),
            right
                .range
                .end()
                .checked_sub(left.range.start())
                .ok_or(MmError::Overflow)?,
            left.range.page_size(),
        )?;
        Ok(Some(Self {
            source: self.source,
            request: UffdRegistrationRequest { range, ..left },
        }))
    }
}

/// One Linux UFFDIO_REGISTER range owned by a single userfaultfd handler.
///
/// The adapter supplies the raw ioctl range once plus every compatible VMA
/// snapshot intersecting it. The MM policy derives canonical post-operation
/// registration fragments; syscall code does not implement interval union.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdRegistrationIntent {
    handler: FaultHandlerId,
    range: PageRange,
    mode: UffdRegisterMode,
}

impl UffdRegistrationIntent {
    pub fn new(
        handler: FaultHandlerId,
        range: PageRange,
        mode: UffdRegisterMode,
    ) -> Result<Self, MmError> {
        if mode != UffdRegisterMode::MISSING {
            return Err(MmError::UnsupportedUffdMode);
        }
        Ok(Self {
            handler,
            range,
            mode,
        })
    }

    pub const fn handler(self) -> FaultHandlerId {
        self.handler
    }

    pub const fn range(self) -> PageRange {
        self.range
    }

    pub const fn mode(self) -> UffdRegisterMode {
        self.mode
    }
}

/// Constant-size proof for a canonical same-handler registration delta.
///
/// After preflight, the adapter reserves exactly the reported output capacity
/// and asks the table to replay removed IDs and replacement requests into that
/// storage. No array proportional to table capacity is hidden in this value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "replay this unchanged delta or discard the proof"]
pub struct UffdRegistrationDeltaPlan {
    revision: u64,
    removed: usize,
    replacements: usize,
    next_id_after: Option<NonZeroU64>,
}

impl UffdRegistrationDeltaPlan {
    /// Existing canonical fragments replaced by the registration.
    pub const fn removed(self) -> usize {
        self.removed
    }

    /// Canonical post-operation fragments that need new identities.
    pub const fn replacements(self) -> usize {
        self.replacements
    }

    /// Whether Linux's same-handler registration is already satisfied.
    pub const fn is_noop(self) -> bool {
        self.removed == 0 && self.replacements == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CanonicalRegistrationDelta {
    desired: UffdRegistrationRequest,
    no_op: bool,
}

/// Small proof that a registration or mapping-refresh batch can commit.
///
/// The proof contains only counters and sequence state; it never copies an
/// array proportional to table capacity onto the kernel stack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "commit the unchanged batch or discard this preflight proof"]
pub struct UffdRegistrationPlan {
    revision: u64,
    removed: usize,
    published: usize,
    reused: usize,
    next_id_after: Option<NonZeroU64>,
}

impl UffdRegistrationPlan {
    /// Existing records removed by a mapping refresh.
    pub const fn removed(self) -> usize {
        self.removed
    }

    /// New non-reused records to publish.
    pub const fn published(self) -> usize {
        self.published
    }

    /// Exact same-handler records reused idempotently.
    pub const fn reused(self) -> usize {
        self.reused
    }

    /// Unique output registrations.
    pub const fn registrations(self) -> usize {
        self.published + self.reused
    }

    const fn changes_table(self) -> bool {
        self.removed != 0 || self.published != 0
    }
}

/// Counts returned after one all-or-none registration-table commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdRegistrationCommit {
    removed: usize,
    published: usize,
    reused: usize,
}

impl UffdRegistrationCommit {
    pub const fn removed(self) -> usize {
        self.removed
    }

    pub const fn published(self) -> usize {
        self.published
    }

    pub const fn reused(self) -> usize {
        self.reused
    }

    pub const fn registrations(self) -> usize {
        self.published + self.reused
    }
}

/// Allocation-free fixed-capacity interval ownership table.
pub struct UffdRegistrationTable<const CAPACITY: usize> {
    records: [Option<UffdRegistration>; CAPACITY],
    next_id: Option<NonZeroU64>,
    revision: u64,
}

impl<const CAPACITY: usize> UffdRegistrationTable<CAPACITY> {
    pub fn new(first_id: u64) -> Result<Self, MmError> {
        Ok(Self {
            records: [None; CAPACITY],
            next_id: Some(NonZeroU64::new(first_id).ok_or(MmError::InvalidIdentity)?),
            revision: 1,
        })
    }

    /// Publishes one interval or returns the existing exact same-handler record.
    pub fn register(
        &mut self,
        api: &UffdApiState,
        request: UffdRegistrationRequest,
    ) -> Result<UffdRegistration, MmError> {
        let mut registration = None;
        self.register_batch(api, core::slice::from_ref(&request), |current| {
            registration = Some(current);
        })?;
        registration.ok_or(MmError::InconsistentAccounting)
    }

    /// Preflights every mapped fragment without changing the table.
    ///
    /// The adapter supplies one canonical post-operation UFFD-owned VMA
    /// fragment per request; gaps produce no request. It must not blindly pass
    /// `ioctl_range intersect pre_state_vma`: Linux treats a same-handler
    /// partial re-registration as a success and may split/merge adjacent UFFD
    /// VMAs. Use [`Self::preflight_register_delta`] to normalize that delta
    /// into exact retained records plus one all-or-none replacement
    /// transaction.
    pub fn preflight_register(
        &self,
        api: &UffdApiState,
        requests: &[UffdRegistrationRequest],
    ) -> Result<UffdRegistrationPlan, MmError> {
        api.require_initialized()?;
        if requests.is_empty() {
            return Err(MmError::ZeroLength);
        }
        self.plan_changes(&[], requests)
    }

    /// Commits an unchanged preflight and visits each unique output record.
    ///
    /// The callback must be infallible. It runs only after table mutation is
    /// complete, so the adapter can publish returned tokens without creating a
    /// partial table transaction.
    pub fn commit_register<F>(
        &mut self,
        plan: UffdRegistrationPlan,
        requests: &[UffdRegistrationRequest],
        visit: F,
    ) -> Result<UffdRegistrationCommit, MmError>
    where
        F: FnMut(UffdRegistration),
    {
        self.commit_changes(plan, &[], requests, visit)
    }

    /// Preflights and atomically registers all mapped fragments.
    ///
    /// Exact same-handler MISSING registrations are idempotent. Any overlap
    /// owned by a different handler returns [MmError::Busy], matching Linux's
    /// EBUSY distinction. A non-exact same-handler overlap is deliberately
    /// rejected as a non-canonical batch; callers use
    /// [`Self::preflight_register_delta`] before invoking this primitive.
    pub fn register_batch<F>(
        &mut self,
        api: &UffdApiState,
        requests: &[UffdRegistrationRequest],
        visit: F,
    ) -> Result<UffdRegistrationCommit, MmError>
    where
        F: FnMut(UffdRegistration),
    {
        let plan = self.preflight_register(api, requests)?;
        self.commit_register(plan, requests, visit)
    }

    /// Plans Linux's same-handler partial UFFDIO_REGISTER interval law.
    ///
    /// The vmas slice contains every compatible current VMA snapshot
    /// intersecting the requested range, in strictly increasing
    /// non-overlapping order. Gaps are allowed and remain gaps. The planner
    /// computes transitive same-handler interval union inside each supplied
    /// VMA, detects foreign ownership, revalidates every folded
    /// mapping/generation lineage, and preflights table capacity, identity
    /// exhaustion, and sealed revision before emitting any delta. A stale
    /// sidecar must first use the explicit mapping-replacement transaction.
    pub fn preflight_register_delta(
        &self,
        api: &UffdApiState,
        intent: UffdRegistrationIntent,
        vmas: &[MappingSnapshot],
    ) -> Result<UffdRegistrationDeltaPlan, MmError> {
        api.require_initialized()?;
        self.plan_register_delta(intent, vmas)
    }

    /// Validates the API gate and current VMA profile for UFFDIO_UNREGISTER.
    ///
    /// `vmas` contains every current mapping fragment intersecting `range` in
    /// strictly increasing, non-overlapping order. At least one compatible
    /// anonymous-private fragment is required; gaps in the raw range are
    /// allowed. The returned address-space identity is common to every
    /// fragment and can bind the adapter's subsequent removal transaction.
    pub fn validate_unregister_vmas(
        &self,
        api: &UffdApiState,
        range: PageRange,
        vmas: &[MappingSnapshot],
    ) -> Result<AddressSpaceId, MmError> {
        api.require_initialized()?;
        Self::validate_vma_profile(range, vmas)
    }

    /// Replays a preflighted canonical delta into caller-owned bounded storage.
    ///
    /// Both callbacks must be infallible. The adapter first checks its storage
    /// against [UffdRegistrationDeltaPlan::removed] and
    /// [UffdRegistrationDeltaPlan::replacements]. If no IDs are removed it
    /// atomically submits the requests through [Self::register_batch];
    /// otherwise it submits both slices through [Self::replace_batch].
    pub fn replay_register_delta<R, P>(
        &self,
        plan: UffdRegistrationDeltaPlan,
        intent: UffdRegistrationIntent,
        vmas: &[MappingSnapshot],
        mut remove: R,
        mut replace: P,
    ) -> Result<(), MmError>
    where
        R: FnMut(UffdRegistrationId),
        P: FnMut(UffdRegistrationRequest),
    {
        if self.revision != plan.revision || self.plan_register_delta(intent, vmas)? != plan {
            return Err(MmError::StaleGeneration);
        }
        for snapshot in vmas.iter().copied() {
            let delta = self.canonical_register_delta(intent, snapshot)?;
            if delta.no_op {
                continue;
            }
            for registration in self.records.iter().flatten().copied() {
                if self.registration_folds_into(
                    intent,
                    snapshot,
                    delta.desired.range(),
                    registration,
                )? {
                    remove(registration.id());
                }
            }
            replace(delta.desired);
        }
        Ok(())
    }

    /// Preflights a mapping split, trim, grow, or generation refresh.
    ///
    /// Every removed token must share one handler/address-space/mode owner, and
    /// every replacement fragment must retain that owner. Old records remain
    /// visible until [Self::commit_replace] succeeds.
    pub fn preflight_replace(
        &self,
        api: &UffdApiState,
        removed: &[UffdRegistrationId],
        replacements: &[UffdRegistrationRequest],
    ) -> Result<UffdRegistrationPlan, MmError> {
        api.require_initialized()?;
        if removed.is_empty() {
            return Err(MmError::ZeroLength);
        }
        self.plan_changes(removed, replacements)
    }

    /// Atomically removes old mapping fragments and publishes replacements.
    pub fn commit_replace<F>(
        &mut self,
        plan: UffdRegistrationPlan,
        removed: &[UffdRegistrationId],
        replacements: &[UffdRegistrationRequest],
        visit: F,
    ) -> Result<UffdRegistrationCommit, MmError>
    where
        F: FnMut(UffdRegistration),
    {
        self.commit_changes(plan, removed, replacements, visit)
    }

    /// Preflights and commits one mapping-fragment replacement transaction.
    pub fn replace_batch<F>(
        &mut self,
        api: &UffdApiState,
        removed: &[UffdRegistrationId],
        replacements: &[UffdRegistrationRequest],
        visit: F,
    ) -> Result<UffdRegistrationCommit, MmError>
    where
        F: FnMut(UffdRegistration),
    {
        let plan = self.preflight_replace(api, removed, replacements)?;
        self.commit_replace(plan, removed, replacements, visit)
    }

    /// Preflights one MM mutation that may cross several handler owners.
    ///
    /// Each replacement names the removed source registration whose
    /// handler/address-space/mode it inherits. A removed token may have zero,
    /// one, or several replacements, covering remove, refresh, trim, grow, and
    /// split transactions.
    pub fn preflight_mapping_replace(
        &self,
        removed: &[UffdRegistrationId],
        replacements: &[UffdRegistrationReplacement],
    ) -> Result<UffdRegistrationPlan, MmError> {
        self.validate_mapping_replacements(removed, replacements)?;
        self.plan_request_changes(
            removed,
            replacements.len(),
            &|index| replacements[index].request,
            false,
        )
    }

    /// Commits an unchanged mixed-owner MM replacement proof.
    pub fn commit_mapping_replace<F>(
        &mut self,
        plan: UffdRegistrationPlan,
        removed: &[UffdRegistrationId],
        replacements: &[UffdRegistrationReplacement],
        visit: F,
    ) -> Result<UffdRegistrationCommit, MmError>
    where
        F: FnMut(UffdRegistration),
    {
        if self.revision != plan.revision {
            return Err(MmError::StaleGeneration);
        }
        self.validate_mapping_replacements(removed, replacements)?;
        self.commit_request_changes(
            plan,
            removed,
            replacements.len(),
            &|index| replacements[index].request,
            false,
            visit,
        )
    }

    /// Preflights and commits one mixed-owner MM replacement transaction.
    pub fn mapping_replace_batch<F>(
        &mut self,
        removed: &[UffdRegistrationId],
        replacements: &[UffdRegistrationReplacement],
        visit: F,
    ) -> Result<UffdRegistrationCommit, MmError>
    where
        F: FnMut(UffdRegistration),
    {
        let plan = self.preflight_mapping_replace(removed, replacements)?;
        self.commit_mapping_replace(plan, removed, replacements, visit)
    }

    /// Preflights a pure multi-owner unmap/unregister removal.
    pub fn preflight_mapping_remove(
        &self,
        removed: &[UffdRegistrationId],
    ) -> Result<UffdRegistrationPlan, MmError> {
        self.preflight_mapping_replace(removed, &[])
    }

    /// Commits a pure multi-owner unmap/unregister removal.
    pub fn commit_mapping_remove(
        &mut self,
        plan: UffdRegistrationPlan,
        removed: &[UffdRegistrationId],
    ) -> Result<UffdRegistrationCommit, MmError> {
        self.commit_mapping_replace(plan, removed, &[], |_| {})
    }

    fn validate_mapping_replacements(
        &self,
        removed: &[UffdRegistrationId],
        replacements: &[UffdRegistrationReplacement],
    ) -> Result<(), MmError> {
        if removed.is_empty() {
            return Err(MmError::ZeroLength);
        }
        let address_space = self.get(removed[0])?.address_space();
        for id in removed.iter().copied() {
            if self.get(id)?.address_space() != address_space {
                return Err(MmError::InvalidUffdRegistrationBatch);
            }
        }
        for replacement in replacements.iter().copied() {
            if !removed.contains(&replacement.source) {
                return Err(MmError::InvalidUffdRegistrationBatch);
            }
            let source = self.get(replacement.source)?;
            let request = replacement.request;
            if source.handler() != request.handler()
                || source.address_space() != request.address_space()
                || source.mode() != request.mode()
            {
                return Err(MmError::InvalidUffdRegistrationBatch);
            }
        }
        Ok(())
    }

    fn plan_register_delta(
        &self,
        intent: UffdRegistrationIntent,
        vmas: &[MappingSnapshot],
    ) -> Result<UffdRegistrationDeltaPlan, MmError> {
        Self::validate_vma_profile(intent.range, vmas)?;

        let mut removed = 0usize;
        let mut replacements = 0usize;
        for snapshot in vmas.iter().copied() {
            let delta = self.canonical_register_delta(intent, snapshot)?;
            if delta.no_op {
                continue;
            }
            replacements = replacements.checked_add(1).ok_or(MmError::Overflow)?;
            for registration in self.records.iter().flatten().copied() {
                if self.registration_folds_into(
                    intent,
                    snapshot,
                    delta.desired.range(),
                    registration,
                )? {
                    removed = removed.checked_add(1).ok_or(MmError::Overflow)?;
                }
            }
        }

        let retained = self
            .len()
            .checked_sub(removed)
            .ok_or(MmError::InconsistentAccounting)?;
        let available = CAPACITY
            .checked_sub(retained)
            .ok_or(MmError::InconsistentAccounting)?;
        if replacements > available {
            return Err(MmError::CapacityExceeded);
        }
        let next_id_after = advance_registration_ids(self.next_id, replacements)?;
        if replacements != 0 && self.revision == u64::MAX {
            return Err(MmError::IdExhausted);
        }
        Ok(UffdRegistrationDeltaPlan {
            revision: self.revision,
            removed,
            replacements,
            next_id_after,
        })
    }

    fn validate_vma_profile(
        range: PageRange,
        vmas: &[MappingSnapshot],
    ) -> Result<AddressSpaceId, MmError> {
        let first = vmas.first().copied().ok_or(MmError::RangeNotMapped)?;
        let address_space = first.address_space();
        let mut previous_end = None;
        for snapshot in vmas.iter().copied() {
            if snapshot.address_space() != address_space
                || snapshot.kind() != MappingKind::AnonymousPrivate
                || snapshot.range().page_size() != range.page_size()
                || previous_end.is_some_and(|end| snapshot.range().start() < end)
                || page_range_intersection(range, snapshot.range()).is_none()
            {
                return Err(MmError::InvalidUffdRegistrationBatch);
            }
            previous_end = Some(snapshot.range().end());
        }
        Ok(address_space)
    }

    fn canonical_register_delta(
        &self,
        intent: UffdRegistrationIntent,
        snapshot: MappingSnapshot,
    ) -> Result<CanonicalRegistrationDelta, MmError> {
        let target = page_range_intersection(intent.range, snapshot.range())
            .ok_or(MmError::RangeNotMapped)?;
        for registration in self.records.iter().flatten().copied() {
            if registration.address_space() == snapshot.address_space()
                && ranges_overlap(registration.range(), target)
            {
                registration.revalidate(snapshot)?;
                if registration.handler() != intent.handler {
                    return Err(MmError::Busy);
                }
            }
        }

        let mut desired = target;
        loop {
            let before = desired;
            for registration in self.records.iter().flatten().copied() {
                if !self.registration_folds_into(intent, snapshot, desired, registration)? {
                    continue;
                }
                let start = desired.start().min(registration.range().start());
                let end = desired.end().max(registration.range().end());
                desired = PageRange::with_page_size(
                    start,
                    end.checked_sub(start).ok_or(MmError::Overflow)?,
                    snapshot.range().page_size(),
                )?;
            }
            if desired == before {
                break;
            }
        }

        let desired = UffdRegistrationRequest::new(intent.handler, snapshot, desired, intent.mode)?;
        let mut connected = 0usize;
        let mut exact = false;
        for registration in self.records.iter().flatten().copied() {
            if self.registration_folds_into(intent, snapshot, desired.range(), registration)? {
                connected = connected.checked_add(1).ok_or(MmError::Overflow)?;
                exact |= registration.request == desired;
            }
        }
        Ok(CanonicalRegistrationDelta {
            desired,
            no_op: connected == 1 && exact,
        })
    }

    fn registration_folds_into(
        &self,
        intent: UffdRegistrationIntent,
        snapshot: MappingSnapshot,
        desired: PageRange,
        registration: UffdRegistration,
    ) -> Result<bool, MmError> {
        if registration.address_space() != snapshot.address_space()
            || registration.handler() != intent.handler
            || registration.mode() != intent.mode
            || !ranges_touch(registration.range(), desired)
            || !ranges_overlap(registration.range(), snapshot.range())
        {
            return Ok(false);
        }
        registration.revalidate(snapshot)?;
        Ok(true)
    }

    fn plan_changes(
        &self,
        removed: &[UffdRegistrationId],
        requests: &[UffdRegistrationRequest],
    ) -> Result<UffdRegistrationPlan, MmError> {
        self.plan_request_changes(removed, requests.len(), &|index| requests[index], true)
    }

    fn plan_request_changes<F>(
        &self,
        removed: &[UffdRegistrationId],
        request_count: usize,
        request_at: &F,
        require_single_owner: bool,
    ) -> Result<UffdRegistrationPlan, MmError>
    where
        F: Fn(usize) -> UffdRegistrationRequest,
    {
        if removed.is_empty() && request_count == 0 {
            return Err(MmError::ZeroLength);
        }
        let mut owner = None;
        for (index, id) in removed.iter().copied().enumerate() {
            if removed[..index].contains(&id) {
                return Err(MmError::InvalidUffdRegistrationBatch);
            }
            let registration = self.get(id)?;
            let current_owner = (
                registration.handler(),
                registration.address_space(),
                registration.mode(),
            );
            if require_single_owner {
                if owner.is_some_and(|expected| expected != current_owner) {
                    return Err(MmError::InvalidUffdRegistrationBatch);
                }
                owner = Some(current_owner);
            }
        }

        let mut unique_requests = 0usize;
        let mut published = 0usize;
        let mut reused = 0usize;
        for index in 0..request_count {
            let request = request_at(index);
            let current_owner = (request.handler, request.address_space, request.mode);
            if require_single_owner {
                if owner.is_some_and(|expected| expected != current_owner) {
                    return Err(MmError::InvalidUffdRegistrationBatch);
                }
                owner = Some(current_owner);
            }

            let mut duplicate = false;
            for previous_index in 0..index {
                let previous = request_at(previous_index);
                duplicate |= previous == request;
                if previous != request && ranges_overlap(previous.range, request.range) {
                    return Err(MmError::InvalidUffdRegistrationBatch);
                }
            }
            if duplicate {
                continue;
            }
            unique_requests = unique_requests.checked_add(1).ok_or(MmError::Overflow)?;

            let mut exact = false;
            for registered in self.records.iter().flatten().copied() {
                if removed.contains(&registered.id)
                    || registered.address_space() != request.address_space
                    || !ranges_overlap(registered.range(), request.range)
                {
                    continue;
                }
                if registered.handler() != request.handler {
                    return Err(MmError::Busy);
                }
                if registered.request == request {
                    exact = true;
                } else {
                    return Err(MmError::UffdRegistrationOverlap);
                }
            }
            if exact {
                reused = reused.checked_add(1).ok_or(MmError::Overflow)?;
            } else {
                published = published.checked_add(1).ok_or(MmError::Overflow)?;
            }
        }

        let retained = self
            .len()
            .checked_sub(removed.len())
            .ok_or(MmError::InconsistentAccounting)?;
        let available = CAPACITY
            .checked_sub(retained)
            .ok_or(MmError::InconsistentAccounting)?;
        if published > available {
            return Err(MmError::CapacityExceeded);
        }
        if unique_requests != published + reused {
            return Err(MmError::InconsistentAccounting);
        }

        let next_id_after = advance_registration_ids(self.next_id, published)?;
        let plan = UffdRegistrationPlan {
            revision: self.revision,
            removed: removed.len(),
            published,
            reused,
            next_id_after,
        };
        // Saturating the optimistic-plan revision must stop transactions that
        // publish new identities, but it must never make pure retirement or
        // final teardown impossible. At MAX no changing plan with new records
        // can be prepared; pure removals remain stale-safe because every
        // commit recomputes the unchanged removed-token set before mutation.
        if plan.published != 0 && self.revision == u64::MAX {
            return Err(MmError::IdExhausted);
        }
        Ok(plan)
    }

    fn commit_changes<F>(
        &mut self,
        plan: UffdRegistrationPlan,
        removed: &[UffdRegistrationId],
        requests: &[UffdRegistrationRequest],
        visit: F,
    ) -> Result<UffdRegistrationCommit, MmError>
    where
        F: FnMut(UffdRegistration),
    {
        self.commit_request_changes(
            plan,
            removed,
            requests.len(),
            &|index| requests[index],
            true,
            visit,
        )
    }

    fn commit_request_changes<R, V>(
        &mut self,
        plan: UffdRegistrationPlan,
        removed: &[UffdRegistrationId],
        request_count: usize,
        request_at: &R,
        require_single_owner: bool,
        mut visit: V,
    ) -> Result<UffdRegistrationCommit, MmError>
    where
        R: Fn(usize) -> UffdRegistrationRequest,
        V: FnMut(UffdRegistration),
    {
        if self.revision != plan.revision
            || self.plan_request_changes(
                removed,
                request_count,
                request_at,
                require_single_owner,
            )? != plan
        {
            return Err(MmError::StaleGeneration);
        }

        for record in &mut self.records {
            if record.is_some_and(|registration| removed.contains(&registration.id)) {
                *record = None;
            }
        }

        let mut next_id = self.next_id;
        for index in 0..request_count {
            let request = request_at(index);
            let duplicate = (0..index).any(|previous| request_at(previous) == request);
            if duplicate
                || self
                    .records
                    .iter()
                    .flatten()
                    .any(|registration| registration.request == request)
            {
                continue;
            }
            let raw = next_id.expect("registration ID availability was preflighted");
            next_id = raw.get().checked_add(1).and_then(NonZeroU64::new);
            let slot = self
                .records
                .iter()
                .position(Option::is_none)
                .expect("registration capacity was preflighted");
            self.records[slot] = Some(UffdRegistration {
                id: UffdRegistrationId(raw),
                request,
            });
        }

        self.next_id = plan.next_id_after;
        if plan.changes_table() && self.revision != u64::MAX {
            self.revision += 1;
        }

        for index in 0..request_count {
            let request = request_at(index);
            if (0..index).any(|previous| request_at(previous) == request) {
                continue;
            }
            let registration = self
                .records
                .iter()
                .flatten()
                .copied()
                .find(|registration| registration.request == request)
                .expect("committed registration must be discoverable");
            visit(registration);
        }

        Ok(UffdRegistrationCommit {
            removed: plan.removed,
            published: plan.published,
            reused: plan.reused,
        })
    }

    /// Every live registration in table-slot order without allocation.
    pub fn iter(&self) -> impl Iterator<Item = UffdRegistration> + '_ {
        self.records.iter().flatten().copied()
    }

    /// Returns the single registration/fault epoch for one logical mapping.
    ///
    /// No matching registration returns `None`. If live fragments disagree on
    /// the epoch, the table fails closed instead of choosing one lineage.
    pub fn epoch_for_mapping(
        &self,
        address_space: AddressSpaceId,
        mapping: MappingId,
    ) -> Result<Option<MappingGeneration>, MmError> {
        let mut epoch = None;
        for registration in self.records.iter().flatten().copied() {
            if registration.address_space() != address_space || registration.mapping() != mapping {
                continue;
            }
            if epoch.is_some_and(|expected| expected != registration.generation()) {
                return Err(MmError::StaleGeneration);
            }
            epoch = Some(registration.generation());
        }
        Ok(epoch)
    }

    /// Live registrations intersecting one address-space range.
    ///
    /// The adapter can collect IDs into its own preallocated buffer before
    /// constructing an all-or-none mapping replacement/removal transaction.
    pub fn intersecting(
        &self,
        address_space: AddressSpaceId,
        range: PageRange,
    ) -> impl Iterator<Item = UffdRegistration> + '_ {
        self.iter().filter(move |registration| {
            registration.address_space() == address_space
                && ranges_overlap(registration.range(), range)
        })
    }

    pub fn get(&self, id: UffdRegistrationId) -> Result<UffdRegistration, MmError> {
        self.records
            .iter()
            .flatten()
            .copied()
            .find(|registration| registration.id == id)
            .ok_or(MmError::UnknownUffdRegistration)
    }

    /// Finds one registration covering a resolver destination.
    ///
    /// No handler is supplied or compared. Linux COPY/ZEROPAGE are
    /// capabilities bound to the address space, not restricted to VMAs owned
    /// by the invoking userfaultfd context.
    pub fn resolver_registration(
        &self,
        address_space: AddressSpaceId,
        destination: PageRange,
    ) -> Result<UffdRegistration, MmError> {
        self.records
            .iter()
            .flatten()
            .copied()
            .find(|registration| {
                registration.address_space() == address_space
                    && registration.range().contains(destination)
            })
            .ok_or(MmError::UnknownUffdRegistration)
    }

    /// Removes one exact registration token.
    ///
    /// The caller's handler identity is intentionally absent: Linux permits a
    /// userfaultfd bound to the same address space to unregister another
    /// userfaultfd context's VMA registration.
    pub fn unregister(&mut self, id: UffdRegistrationId) -> Result<UffdRegistration, MmError> {
        let slot = self
            .records
            .iter()
            .position(|record| record.is_some_and(|registration| registration.id == id))
            .ok_or(MmError::UnknownUffdRegistration)?;
        let registration = self.records[slot]
            .take()
            .ok_or(MmError::UnknownUffdRegistration)?;
        self.revision = self.revision.saturating_add(1);
        Ok(registration)
    }

    /// Detaches every interval owned by one closing handler.
    pub fn detach_handler(&mut self, handler: FaultHandlerId) -> Result<usize, MmError> {
        let removed = self
            .records
            .iter()
            .flatten()
            .filter(|registration| registration.handler() == handler)
            .count();
        if removed == 0 {
            return Ok(0);
        }
        for record in &mut self.records {
            if record.is_some_and(|registration| registration.handler() == handler) {
                *record = None;
            }
        }
        self.revision = self.revision.saturating_add(1);
        Ok(removed)
    }

    pub fn len(&self) -> usize {
        self.records.iter().flatten().count()
    }

    pub const fn is_empty(&self) -> bool {
        let mut index = 0;
        while index < CAPACITY {
            if self.records[index].is_some() {
                return false;
            }
            index += 1;
        }
        true
    }
}

fn advance_registration_ids(
    mut next: Option<NonZeroU64>,
    count: usize,
) -> Result<Option<NonZeroU64>, MmError> {
    for _ in 0..count {
        let raw = next.ok_or(MmError::IdExhausted)?;
        next = raw.get().checked_add(1).and_then(NonZeroU64::new);
    }
    Ok(next)
}

fn ranges_overlap(left: PageRange, right: PageRange) -> bool {
    left.start() < right.end() && right.start() < left.end()
}

fn ranges_touch(left: PageRange, right: PageRange) -> bool {
    left.page_size() == right.page_size()
        && left.start() <= right.end()
        && right.start() <= left.end()
}

fn page_range_intersection(left: PageRange, right: PageRange) -> Option<PageRange> {
    if left.page_size() != right.page_size() {
        return None;
    }
    let start = left.start().max(right.start());
    let end = left.end().min(right.end());
    if start >= end {
        return None;
    }
    PageRange::with_page_size(start, end - start, left.page_size()).ok()
}

/// Stateless Linux MISSING-fault policy over the lower generic broker seam.
///
/// The lower broker is the sole owner of queue storage, FIFO claim,
/// Pending/Delivered state, waiters, readiness, credits, cancellation,
/// terminal publication, and close. This policy only validates Linux
/// registration/mapping facts and returns typed lower-layer permits.
pub struct UffdFaultPolicy;

impl UffdFaultPolicy {
    /// Validates registration ownership and typed lower-broker admission.
    pub fn admit(
        registration: UffdRegistration,
        current: MappingSnapshot,
        request: FaultRequest,
        context: FaultAdmissionContext,
        lifecycle: FaultLifecycleState,
    ) -> Result<FaultAdmissionPermit, MmError> {
        registration.validate_fault_admission(current, request)?;
        FaultAdmission::check(request, current, context, lifecycle)
    }

    /// Revalidates one lower ticket's identity and page coverage before
    /// irreversible resolution, without rechecking post-admission access.
    pub fn prepare_completion(
        request: FaultRequest,
        current: MappingSnapshot,
        disposition: FaultDisposition,
    ) -> Result<FaultCompletionPermit, MmError> {
        if !matches!(
            disposition,
            FaultDisposition::Supply | FaultDisposition::ZeroFill | FaultDisposition::Failure(_)
        ) {
            return Err(MmError::UnsupportedUffdDisposition);
        }
        validate_fault_completion(request, current, disposition)
    }
}

/// Checked Linux v6.12 UFFDIO_COPY mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdCopyMode(u64);

impl UffdCopyMode {
    const DONTWAKE: u64 = 1 << 0;
    const WP: u64 = 1 << 1;
    const LINUX_V6_12: u64 = Self::DONTWAKE | Self::WP;

    /// Accepts every Linux v6.12 COPY mode bit.
    ///
    /// This pure-policy type distinguishes a recognized request from an
    /// unknown raw bit. A consumer that has not implemented write-protected
    /// publication must reject [`Self::write_protect`] only after resolving
    /// the target-mm/range error precedence and must report that lower error
    /// through Linux's signed `uffdio_copy.copy` result field.
    pub const fn from_bits(bits: u64) -> Result<Self, MmError> {
        if bits & !Self::LINUX_V6_12 != 0 {
            return Err(MmError::InvalidUffdCopyMode);
        }
        Ok(Self(bits))
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn dontwake(self) -> bool {
        self.0 & Self::DONTWAKE != 0
    }

    /// Requests atomic write protection together with page installation.
    pub const fn write_protect(self) -> bool {
        self.0 & Self::WP != 0
    }
}

/// Checked first-profile UFFDIO_ZEROPAGE mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdZeroPageMode(u64);

impl UffdZeroPageMode {
    const DONTWAKE: u64 = 1 << 0;

    /// Accepts zero or DONTWAKE.
    pub const fn from_bits(bits: u64) -> Result<Self, MmError> {
        if bits & !Self::DONTWAKE != 0 {
            return Err(MmError::InvalidUffdZeroPageMode);
        }
        Ok(Self(bits))
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn dontwake(self) -> bool {
        self.0 & Self::DONTWAKE != 0
    }
}

/// Validated COPY geometry and wake policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdCopyRequest {
    source: UserRange,
    destination: PageRange,
    mode: UffdCopyMode,
}

impl UffdCopyRequest {
    pub const fn new(
        source: UserRange,
        destination: PageRange,
        mode: UffdCopyMode,
    ) -> Result<Self, MmError> {
        if source.len() != destination.len() {
            return Err(MmError::InconsistentAccounting);
        }
        if !destination.page_size().is_aligned(source.len()) {
            return Err(MmError::Unaligned);
        }
        Ok(Self {
            source,
            destination,
            mode,
        })
    }

    pub const fn source(self) -> UserRange {
        self.source
    }

    pub const fn destination(self) -> PageRange {
        self.destination
    }

    pub const fn mode(self) -> UffdCopyMode {
        self.mode
    }
}

/// Validated ZEROPAGE geometry and wake policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdZeroPageRequest {
    destination: PageRange,
    mode: UffdZeroPageMode,
}

impl UffdZeroPageRequest {
    pub const fn new(destination: PageRange, mode: UffdZeroPageMode) -> Self {
        Self { destination, mode }
    }

    pub const fn destination(self) -> PageRange {
        self.destination
    }

    pub const fn mode(self) -> UffdZeroPageMode {
        self.mode
    }
}

/// Linux ioctl return class after signed progress has been copied out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UffdResolverOutcome {
    /// Every requested byte was installed and ioctl returns zero.
    Complete,
    /// A positive prefix was installed and ioctl returns EAGAIN.
    Retry,
    /// No page was installed and ioctl returns the lower mapped error.
    Failed,
}

/// Signed COPY/ZEROPAGE result plus post-copyout wake plan.
///
/// Positive progress is always a page-aligned prefix. A lower failure is
/// represented by a negative Linux errno. The adapter must copy
/// [Self::reported_bytes] first; a failed result copyout preserves installed
/// pages and suppresses wake, exactly as Linux v6.12 does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UffdResolverResult {
    reported_bytes: i64,
    completed: Option<PageRange>,
    outcome: UffdResolverOutcome,
    wake: bool,
}

impl UffdResolverResult {
    /// Classifies positive COPY progress.
    pub fn for_copy(request: UffdCopyRequest, completed: usize) -> Result<Self, MmError> {
        Self::progress(request.destination, completed, request.mode.dontwake())
    }

    /// Classifies positive ZEROPAGE progress.
    pub fn for_zeropage(request: UffdZeroPageRequest, completed: usize) -> Result<Self, MmError> {
        Self::progress(request.destination, completed, request.mode.dontwake())
    }

    /// Builds the signed negative output for a zero-progress lower failure.
    ///
    /// errno is supplied after the syscall adapter maps its lower error and
    /// must be a positive Linux errno number.
    pub const fn failure(errno: i32) -> Result<Self, MmError> {
        if errno <= 0 {
            return Err(MmError::InvalidUffdProgress);
        }
        Ok(Self {
            reported_bytes: -(errno as i64),
            completed: None,
            outcome: UffdResolverOutcome::Failed,
            wake: false,
        })
    }

    fn progress(destination: PageRange, completed: usize, dontwake: bool) -> Result<Self, MmError> {
        if completed == 0
            || completed > destination.len()
            || !destination.page_size().is_aligned(completed)
        {
            return Err(MmError::InvalidUffdProgress);
        }
        let completed_range = destination.subrange(0, completed)?;
        let reported_bytes = i64::try_from(completed).map_err(|_| MmError::Overflow)?;
        let outcome = if completed == destination.len() {
            UffdResolverOutcome::Complete
        } else {
            UffdResolverOutcome::Retry
        };
        Ok(Self {
            reported_bytes,
            completed: Some(completed_range),
            outcome,
            wake: !dontwake,
        })
    }

    /// Signed value written to uffdio_copy.copy or uffdio_zeropage.zeropage.
    pub const fn reported_bytes(self) -> i64 {
        self.reported_bytes
    }

    /// Prefix made irreversible by the lower page installer.
    pub const fn completed(self) -> Option<PageRange> {
        self.completed
    }

    /// Ioctl return class after successful result copyout.
    pub const fn outcome(self) -> UffdResolverOutcome {
        self.outcome
    }

    /// Range to wake only after successful result copyout.
    pub const fn wake_range(self) -> Option<PageRange> {
        if self.wake { self.completed } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MappingAccess, MappingKind, MappingSnapshot};

    const PAGE: usize = 4096;

    fn initialized_api() -> UffdApiState {
        let mut api = UffdApiState::new();
        let negotiation = api.prepare_raw(UFFD_API, 0).unwrap();
        api.commit(negotiation).unwrap();
        api
    }

    fn snapshot(mapping: u64, start: usize, length: usize) -> MappingSnapshot {
        MappingSnapshot::from_raw(
            1,
            mapping,
            1,
            start,
            length,
            PAGE,
            MappingAccess::new(true, true, false).bits(),
            MappingKind::AnonymousPrivate,
            true,
            false,
        )
        .unwrap()
    }

    fn request(handler: u64, mapping: u64, start: usize) -> UffdRegistrationRequest {
        let snapshot = snapshot(mapping, start, PAGE);
        UffdRegistrationRequest::new(
            FaultHandlerId::new(handler).unwrap(),
            snapshot,
            snapshot.range(),
            UffdRegisterMode::MISSING,
        )
        .unwrap()
    }

    #[test]
    fn saturated_revision_still_allows_retirement_and_final_detach() {
        let api = initialized_api();
        let mut table = UffdRegistrationTable::<3>::new(1).unwrap();
        let first = table.register(&api, request(10, 20, 0x1000)).unwrap();
        let second = table.register(&api, request(11, 21, 0x3000)).unwrap();
        table.revision = u64::MAX;

        let plan = table.preflight_mapping_remove(&[first.id()]).unwrap();
        let commit = table.commit_mapping_remove(plan, &[first.id()]).unwrap();
        assert_eq!(commit.removed(), 1);
        assert_eq!(table.revision, u64::MAX);
        assert_eq!(table.get(first.id()), Err(MmError::UnknownUffdRegistration));

        assert_eq!(table.detach_handler(second.handler()).unwrap(), 1);
        assert!(table.is_empty());
        assert_eq!(table.revision, u64::MAX);
    }

    #[test]
    fn saturated_revision_retire_plans_commute_but_never_consume_a_token_twice() {
        let api = initialized_api();
        let mut table = UffdRegistrationTable::<3>::new(1).unwrap();
        let first = table.register(&api, request(10, 20, 0x1000)).unwrap();
        let second = table.register(&api, request(11, 21, 0x3000)).unwrap();
        table.revision = u64::MAX;

        let first_plan = table.preflight_mapping_remove(&[first.id()]).unwrap();
        let duplicate_first_plan = table.preflight_mapping_remove(&[first.id()]).unwrap();
        let second_plan = table.preflight_mapping_remove(&[second.id()]).unwrap();

        table
            .commit_mapping_remove(first_plan, &[first.id()])
            .unwrap();
        assert_eq!(
            table.commit_mapping_remove(duplicate_first_plan, &[first.id()]),
            Err(MmError::UnknownUffdRegistration)
        );
        assert_eq!(table.get(second.id()), Ok(second));
        table
            .commit_mapping_remove(second_plan, &[second.id()])
            .unwrap();
        assert!(table.is_empty());
        assert_eq!(table.revision, u64::MAX);
    }

    #[test]
    fn saturated_revision_rejects_publication_but_not_unregister() {
        let api = initialized_api();
        let mut table = UffdRegistrationTable::<2>::new(1).unwrap();
        let first = table.register(&api, request(10, 20, 0x1000)).unwrap();
        table.revision = u64::MAX;

        assert_eq!(
            table.register(&api, request(10, 21, 0x3000)),
            Err(MmError::IdExhausted)
        );
        assert_eq!(table.unregister(first.id()).unwrap(), first);
        assert!(table.is_empty());
        assert_eq!(table.revision, u64::MAX);
    }

    #[test]
    fn saturated_revision_allows_register_noop_but_rejects_delta_publication() {
        let api = initialized_api();
        let mapping = snapshot(30, 0x5000, 3 * PAGE);
        let mut table = UffdRegistrationTable::<2>::new(1).unwrap();
        let registered = table
            .register(
                &api,
                UffdRegistrationRequest::new(
                    FaultHandlerId::new(12).unwrap(),
                    mapping,
                    PageRange::new(0x5000, 2 * PAGE, PAGE).unwrap(),
                    UffdRegisterMode::MISSING,
                )
                .unwrap(),
            )
            .unwrap();
        table.revision = u64::MAX;

        let subset = UffdRegistrationIntent::new(
            registered.handler(),
            PageRange::new(0x6000, PAGE, PAGE).unwrap(),
            UffdRegisterMode::MISSING,
        )
        .unwrap();
        let plan = table
            .preflight_register_delta(&api, subset, &[mapping])
            .unwrap();
        assert!(plan.is_noop());

        let extension = UffdRegistrationIntent::new(
            registered.handler(),
            mapping.range(),
            UffdRegisterMode::MISSING,
        )
        .unwrap();
        assert_eq!(
            table.preflight_register_delta(&api, extension, &[mapping]),
            Err(MmError::IdExhausted)
        );
        assert_eq!(table.get(registered.id()), Ok(registered));
        assert_eq!(table.revision, u64::MAX);
    }

    #[test]
    fn canonical_replacement_union_does_not_fold_different_modes() {
        let post_vma = snapshot(30, 0x5000, 2 * PAGE);
        let left = UffdRegistrationReplacement::new(
            UffdRegistrationId::new(1).unwrap(),
            request(12, 30, 0x5000),
        );
        let right = UffdRegistrationReplacement::new(
            UffdRegistrationId::new(2).unwrap(),
            UffdRegistrationRequest {
                mode: UffdRegisterMode::WP,
                ..request(12, 30, 0x6000)
            },
        );

        assert_eq!(left.canonical_union(right, post_vma), Ok(None));
    }
}
