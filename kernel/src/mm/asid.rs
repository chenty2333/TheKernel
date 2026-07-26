//! Bounded hardware address-space identifiers for user page tables.
//!
//! Numeric hardware ASIDs are unique for the entire boot.  The current global
//! TLB grace protocol does not stop a remote CPU from refilling an old ASID
//! after it acknowledges a shootdown, so it is not sufficient to make numeric
//! reuse safe.  Once the probed ASID space is exhausted, every later address
//! space receives the legacy ASID-0 token and takes the full-flush path.

use axhal::context::AddressSpaceFallbackReason;
use memory_addr::PhysAddr;

const LEGACY_ASID: usize = 0;
const LEGACY_GENERATION: u64 = 0;
#[cfg(any(
    test,
    all(
        feature = "asid-fast-switch",
        any(target_arch = "riscv64", target_arch = "loongarch64")
    )
))]
const BOOT_GENERATION: u64 = 1;

/// Hardware address-space identity carried by one user page-table root.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HardwareAddressSpaceId {
    asid: usize,
    generation: u64,
    fallback_reason: AddressSpaceFallbackReason,
}

impl HardwareAddressSpaceId {
    /// The conservative ASID-0 identity.
    pub(crate) const fn legacy() -> Self {
        Self::fallback(AddressSpaceFallbackReason::AsidZero)
    }

    /// A conservative identity carrying its exact allocator failure.
    const fn fallback(fallback_reason: AddressSpaceFallbackReason) -> Self {
        Self {
            asid: LEGACY_ASID,
            generation: LEGACY_GENERATION,
            fallback_reason,
        }
    }

    #[cfg(any(
        test,
        all(
            feature = "asid-fast-switch",
            any(target_arch = "riscv64", target_arch = "loongarch64")
        )
    ))]
    const fn new(asid: usize, generation: u64) -> Self {
        Self {
            asid,
            generation,
            fallback_reason: AddressSpaceFallbackReason::None,
        }
    }

    /// Returns the numeric hardware ASID.
    pub(crate) const fn asid(self) -> usize {
        self.asid
    }

    /// Returns the non-wrapping allocator generation.
    pub(crate) const fn generation(self) -> u64 {
        self.generation
    }

    /// Returns why this identity uses ASID 0.
    pub(crate) const fn fallback_reason(self) -> AddressSpaceFallbackReason {
        self.fallback_reason
    }
}

/// User page-table root plus its hardware address-space identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddressSpaceToken {
    root: PhysAddr,
    id: HardwareAddressSpaceId,
}

impl AddressSpaceToken {
    /// Creates a conservative ASID-0 token.
    pub const fn legacy(root: PhysAddr) -> Self {
        Self {
            root,
            id: HardwareAddressSpaceId::legacy(),
        }
    }

    /// Creates a token for an allocated hardware address-space identity.
    pub(crate) const fn new(root: PhysAddr, id: HardwareAddressSpaceId) -> Self {
        Self { root, id }
    }

    /// Returns the page-table root physical address.
    pub const fn root(self) -> PhysAddr {
        self.root
    }

    /// Returns the numeric hardware ASID.
    pub const fn asid(self) -> usize {
        self.id.asid()
    }

    /// Returns the allocator generation.
    pub const fn generation(self) -> u64 {
        self.id.generation()
    }

    /// Returns why this token uses ASID 0.
    pub const fn fallback_reason(self) -> AddressSpaceFallbackReason {
        self.id.fallback_reason()
    }
}

#[cfg(any(
    test,
    all(
        feature = "asid-fast-switch",
        any(target_arch = "riscv64", target_arch = "loongarch64")
    )
))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReserveDecision {
    Assigned(HardwareAddressSpaceId),
    Legacy(AddressSpaceFallbackReason),
}

#[cfg(any(
    test,
    all(
        feature = "asid-fast-switch",
        any(target_arch = "riscv64", target_arch = "loongarch64")
    )
))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AllocatorStatus {
    Disabled,
    InvalidWidth,
    Active,
    Exhausted,
}

#[cfg(any(
    test,
    all(
        feature = "asid-fast-switch",
        any(target_arch = "riscv64", target_arch = "loongarch64")
    )
))]
/// Pure bounded allocator state.  Numeric identifiers are never recycled.
#[derive(Debug)]
struct AllocatorState {
    capacity: usize,
    next_asid: usize,
    status: AllocatorStatus,
}

#[cfg(any(
    test,
    all(
        feature = "asid-fast-switch",
        any(target_arch = "riscv64", target_arch = "loongarch64")
    )
))]
impl AllocatorState {
    const fn disabled() -> Self {
        Self {
            capacity: 0,
            next_asid: 1,
            status: AllocatorStatus::Disabled,
        }
    }

    fn enable(&mut self, capacity: usize) -> bool {
        if capacity == 0 || self.status != AllocatorStatus::Disabled {
            return false;
        }
        self.capacity = capacity;
        self.next_asid = 1;
        self.status = AllocatorStatus::Active;
        true
    }

    fn reject_invalid_width(&mut self) -> bool {
        if self.status != AllocatorStatus::Disabled {
            return false;
        }
        self.status = AllocatorStatus::InvalidWidth;
        true
    }

    fn reserve(&mut self) -> ReserveDecision {
        match self.status {
            AllocatorStatus::Disabled => {
                return ReserveDecision::Legacy(AddressSpaceFallbackReason::AsidZero);
            }
            AllocatorStatus::InvalidWidth => {
                return ReserveDecision::Legacy(AddressSpaceFallbackReason::InvalidWidth);
            }
            AllocatorStatus::Exhausted => {
                return ReserveDecision::Legacy(AddressSpaceFallbackReason::Exhausted);
            }
            AllocatorStatus::Active => {}
        }

        if self.next_asid <= self.capacity {
            let asid = self.next_asid;
            self.next_asid += 1;
            return ReserveDecision::Assigned(HardwareAddressSpaceId::new(asid, BOOT_GENERATION));
        }

        // A global flush/grace is not a quiescence protocol: a target CPU may
        // refill its old ASID after acknowledging the request.  Do not reuse
        // any numeric identifier until a future protocol can first move every
        // running old context to ASID 0 (or otherwise stop those refills).
        self.status = AllocatorStatus::Exhausted;
        ReserveDecision::Legacy(AddressSpaceFallbackReason::Exhausted)
    }
}

#[cfg(all(
    feature = "asid-fast-switch",
    any(target_arch = "riscv64", target_arch = "loongarch64")
))]
static ALLOCATOR: kspin::SpinNoIrq<AllocatorState> =
    kspin::SpinNoIrq::new(AllocatorState::disabled());

#[cfg(any(
    test,
    all(
        feature = "asid-fast-switch",
        any(target_arch = "riscv64", target_arch = "loongarch64")
    )
))]
fn capacity_for_width_and_max(width: usize, architectural_max: usize) -> usize {
    // Reject an unexpected report rather than silently truncating an identity
    // in the hardware register and colliding two page-table roots.
    if width == 0 || width > architectural_max {
        return 0;
    }
    (1usize << width) - 1
}

#[cfg(all(feature = "asid-fast-switch", target_arch = "riscv64"))]
fn capacity_for_width(width: usize) -> usize {
    capacity_for_width_and_max(width, 16)
}

#[cfg(all(feature = "asid-fast-switch", target_arch = "loongarch64"))]
fn capacity_for_width(width: usize) -> usize {
    capacity_for_width_and_max(width, 10)
}

/// Initializes the opt-in hardware-ASID allocator after TLB shootdown setup.
pub(crate) fn init() {
    #[cfg(all(
        feature = "asid-fast-switch",
        any(target_arch = "riscv64", target_arch = "loongarch64")
    ))]
    {
        let capacity = capacity_for_width(axhal::asm::probe_asid_width());
        if capacity == 0 {
            assert!(
                ALLOCATOR.lock().reject_invalid_width(),
                "hardware ASID allocator initialized more than once"
            );
        } else {
            assert!(
                ALLOCATOR.lock().enable(capacity),
                "hardware ASID allocator initialized more than once"
            );
        }
    }
}

/// Reserves a unique nonzero identity or returns the safe ASID-0 fallback.
pub(super) fn reserve_hardware_address_space_id() -> HardwareAddressSpaceId {
    #[cfg(all(
        feature = "asid-fast-switch",
        any(target_arch = "riscv64", target_arch = "loongarch64")
    ))]
    {
        let decision = ALLOCATOR.lock().reserve();
        match decision {
            ReserveDecision::Assigned(id) => return id,
            ReserveDecision::Legacy(reason) => return HardwareAddressSpaceId::fallback(reason),
        }
    }

    #[allow(unreachable_code)]
    HardwareAddressSpaceId::legacy()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonzero_ids_are_unique_within_generation() {
        let mut allocator = AllocatorState::disabled();
        assert!(allocator.enable(3));
        assert_eq!(
            allocator.reserve(),
            ReserveDecision::Assigned(HardwareAddressSpaceId::new(1, 1))
        );
        assert_eq!(
            allocator.reserve(),
            ReserveDecision::Assigned(HardwareAddressSpaceId::new(2, 1))
        );
        assert_eq!(
            allocator.reserve(),
            ReserveDecision::Assigned(HardwareAddressSpaceId::new(3, 1))
        );
    }

    #[test]
    fn exhaustion_permanently_falls_back_without_numeric_reuse() {
        let mut allocator = AllocatorState::disabled();
        assert!(allocator.enable(1));
        assert_eq!(
            allocator.reserve(),
            ReserveDecision::Assigned(HardwareAddressSpaceId::new(1, BOOT_GENERATION))
        );
        assert_eq!(
            allocator.reserve(),
            ReserveDecision::Legacy(AddressSpaceFallbackReason::Exhausted)
        );
        assert_eq!(allocator.status, AllocatorStatus::Exhausted);
        assert_eq!(
            allocator.reserve(),
            ReserveDecision::Legacy(AddressSpaceFallbackReason::Exhausted)
        );
        assert_eq!(
            allocator.reserve(),
            ReserveDecision::Legacy(AddressSpaceFallbackReason::Exhausted)
        );
    }

    #[test]
    fn exhausted_allocator_cannot_be_reenabled() {
        let mut allocator = AllocatorState::disabled();
        assert!(allocator.enable(1));
        let _ = allocator.reserve();
        assert_eq!(
            allocator.reserve(),
            ReserveDecision::Legacy(AddressSpaceFallbackReason::Exhausted)
        );
        assert!(!allocator.enable(1));
        assert_eq!(
            allocator.reserve(),
            ReserveDecision::Legacy(AddressSpaceFallbackReason::Exhausted)
        );
    }

    #[test]
    fn every_assigned_identity_stays_in_the_single_boot_generation() {
        let mut allocator = AllocatorState::disabled();
        assert!(allocator.enable(3));
        for expected_asid in 1..=3 {
            assert_eq!(
                allocator.reserve(),
                ReserveDecision::Assigned(HardwareAddressSpaceId::new(
                    expected_asid,
                    BOOT_GENERATION
                ))
            );
        }
        assert_eq!(
            allocator.reserve(),
            ReserveDecision::Legacy(AddressSpaceFallbackReason::Exhausted)
        );
        assert_eq!(allocator.status, AllocatorStatus::Exhausted);
    }

    #[test]
    fn legacy_tokens_are_never_nonzero() {
        let token = AddressSpaceToken::legacy(PhysAddr::from_usize(0x1000));
        assert_eq!(token.asid(), 0);
        assert_eq!(token.generation(), 0);
        assert_eq!(
            token.fallback_reason(),
            AddressSpaceFallbackReason::AsidZero
        );
    }

    #[test]
    fn reported_width_must_fit_the_architectural_field() {
        assert_eq!(capacity_for_width_and_max(0, 16), 0);
        assert_eq!(capacity_for_width_and_max(16, 16), 65_535);
        assert_eq!(capacity_for_width_and_max(17, 16), 0);
        assert_eq!(capacity_for_width_and_max(10, 10), 1_023);
        assert_eq!(capacity_for_width_and_max(11, 10), 0);
    }

    #[test]
    fn invalid_width_is_preserved_in_every_fallback_identity() {
        let mut allocator = AllocatorState::disabled();
        assert!(allocator.reject_invalid_width());
        assert_eq!(
            allocator.reserve(),
            ReserveDecision::Legacy(AddressSpaceFallbackReason::InvalidWidth)
        );
        assert!(!allocator.enable(1));
    }
}
