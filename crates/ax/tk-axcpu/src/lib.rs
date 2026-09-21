#![cfg_attr(not(test), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
#![doc = include_str!("../README.md")]

#[macro_use]
extern crate log;

#[macro_use]
extern crate memory_addr;

/// Why a page-table identity has no usable nonzero hardware ASID.
///
/// The value is carried with the identity so the switch path never has to
/// reconstruct allocator history from an ASID-0 number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AddressSpaceFallbackReason {
    /// The identity has a valid nonzero hardware ASID.
    None,
    /// The caller deliberately requested the conservative ASID-0 path.
    #[default]
    AsidZero,
    /// Hardware reported an unusable ASID field width.
    InvalidWidth,
    /// The boot-scoped, non-recycling ASID allocator is exhausted.
    Exhausted,
}

/// Classified reason that one address-space switch required a full TLB flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsidSwitchFallbackReason {
    /// At least one identity deliberately uses ASID 0.
    AsidZero,
    /// At least one identity came from an invalid hardware width report.
    InvalidWidth,
    /// At least one identity came from the exhausted bounded allocator.
    Exhausted,
    /// Two nonzero identities belong to different allocator generations.
    GenerationMismatch,
    /// One nonzero numeric ASID names two different page-table roots.
    SameIdDifferentRoot,
}

#[cfg(feature = "asid-fast-switch")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlbSwitchDecision {
    Retain,
    Flush(AsidSwitchFallbackReason),
}

#[cfg(feature = "asid-fast-switch")]
#[inline]
const fn legal_nonzero_identity(
    root: usize,
    asid: usize,
    generation: u64,
    fallback: AddressSpaceFallbackReason,
) -> bool {
    asid < 4096
        && asid != 0
        && generation != 0
        && matches!(fallback, AddressSpaceFallbackReason::None)
        && root & 0xfff == 0
        && root < (1usize << 52)
}

#[cfg(feature = "asid-fast-switch")]
#[inline]
#[allow(clippy::too_many_arguments)]
const fn classify_user_tlb_switch(
    current_root: usize,
    current_asid: usize,
    current_generation: u64,
    current_fallback: AddressSpaceFallbackReason,
    next_root: usize,
    next_asid: usize,
    next_generation: u64,
    next_fallback: AddressSpaceFallbackReason,
) -> TlbSwitchDecision {
    // This predicate relies on the caller never recycling a nonzero numeric
    // ASID during the boot.  TheKernel's bounded allocator permanently falls
    // back to ASID 0 on exhaustion because its current global TLB grace is not
    // a quiescence protocol.  A legal target identity is safe to enter with
    // CR3.NOFLUSH even when the old context is legacy PCID 0: no old PCID-0
    // translation can be selected by the new nonzero PCID.
    let next_is_legacy = next_asid == 0;
    if next_is_legacy {
        let reason = if matches!(current_fallback, AddressSpaceFallbackReason::InvalidWidth)
            || matches!(next_fallback, AddressSpaceFallbackReason::InvalidWidth)
        {
            AsidSwitchFallbackReason::InvalidWidth
        } else if matches!(current_fallback, AddressSpaceFallbackReason::Exhausted)
            || matches!(next_fallback, AddressSpaceFallbackReason::Exhausted)
        {
            AsidSwitchFallbackReason::Exhausted
        } else {
            AsidSwitchFallbackReason::AsidZero
        };
        return TlbSwitchDecision::Flush(reason);
    }

    if !legal_nonzero_identity(next_root, next_asid, next_generation, next_fallback) {
        return TlbSwitchDecision::Flush(AsidSwitchFallbackReason::InvalidWidth);
    }
    if current_asid != 0
        && !legal_nonzero_identity(
            current_root,
            current_asid,
            current_generation,
            current_fallback,
        )
    {
        return TlbSwitchDecision::Flush(AsidSwitchFallbackReason::InvalidWidth);
    }
    if current_asid != 0 && current_generation != next_generation {
        return TlbSwitchDecision::Flush(AsidSwitchFallbackReason::GenerationMismatch);
    }
    if current_asid != 0 && current_asid == next_asid && current_root != next_root {
        return TlbSwitchDecision::Flush(AsidSwitchFallbackReason::SameIdDifferentRoot);
    }
    TlbSwitchDecision::Retain
}

#[macro_use]
pub mod trap;

#[cfg(feature = "uspace")]
mod uspace_common;

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::*;

#[cfg(not(target_arch = "x86_64"))]
compile_error!("axcpu supports only x86_64");

#[cfg(all(test, feature = "asid-fast-switch"))]
mod tests {
    use super::*;

    #[test]
    fn distinct_nonzero_ids_in_one_non_reused_generation_retain_tlb() {
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                1,
                1,
                AddressSpaceFallbackReason::None,
                0x2000,
                2,
                1,
                AddressSpaceFallbackReason::None,
            ),
            TlbSwitchDecision::Retain
        );
    }

    #[test]
    fn legacy_or_cross_generation_transitions_require_full_flush() {
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                0,
                0,
                AddressSpaceFallbackReason::AsidZero,
                0x2000,
                1,
                1,
                AddressSpaceFallbackReason::None,
            ),
            TlbSwitchDecision::Retain
        );
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                1,
                1,
                AddressSpaceFallbackReason::None,
                0x2000,
                2,
                2,
                AddressSpaceFallbackReason::None,
            ),
            TlbSwitchDecision::Flush(AsidSwitchFallbackReason::GenerationMismatch)
        );
    }

    #[test]
    fn same_numeric_id_with_a_different_root_requires_full_flush() {
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                1,
                1,
                AddressSpaceFallbackReason::None,
                0x2000,
                1,
                1,
                AddressSpaceFallbackReason::None,
            ),
            TlbSwitchDecision::Flush(AsidSwitchFallbackReason::SameIdDifferentRoot)
        );
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                1,
                1,
                AddressSpaceFallbackReason::None,
                0x1000,
                1,
                1,
                AddressSpaceFallbackReason::None,
            ),
            TlbSwitchDecision::Retain
        );
    }

    #[test]
    fn zero_identity_preserves_allocator_failure_reason() {
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                0,
                0,
                AddressSpaceFallbackReason::Exhausted,
                0x2000,
                0,
                0,
                AddressSpaceFallbackReason::InvalidWidth,
            ),
            TlbSwitchDecision::Flush(AsidSwitchFallbackReason::InvalidWidth)
        );
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                0,
                0,
                AddressSpaceFallbackReason::Exhausted,
                0x2000,
                1,
                1,
                AddressSpaceFallbackReason::None,
            ),
            TlbSwitchDecision::Retain
        );
    }

    #[test]
    fn legacy_targets_and_invalid_current_identities_flush_defensively() {
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                1,
                1,
                AddressSpaceFallbackReason::None,
                0x2000,
                0,
                0,
                AddressSpaceFallbackReason::AsidZero,
            ),
            TlbSwitchDecision::Flush(AsidSwitchFallbackReason::AsidZero)
        );
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                4096,
                1,
                AddressSpaceFallbackReason::None,
                0x2000,
                2,
                1,
                AddressSpaceFallbackReason::None,
            ),
            TlbSwitchDecision::Flush(AsidSwitchFallbackReason::InvalidWidth)
        );
    }

    #[test]
    fn target_pcids_require_aligned_roots_and_twelve_bit_values() {
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                1,
                1,
                AddressSpaceFallbackReason::None,
                0x1000,
                4096,
                1,
                AddressSpaceFallbackReason::None,
            ),
            TlbSwitchDecision::Flush(AsidSwitchFallbackReason::InvalidWidth)
        );
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                1,
                1,
                AddressSpaceFallbackReason::None,
                0x1001,
                2,
                1,
                AddressSpaceFallbackReason::None,
            ),
            TlbSwitchDecision::Flush(AsidSwitchFallbackReason::InvalidWidth)
        );
        assert_eq!(
            classify_user_tlb_switch(
                0x1000,
                1,
                1,
                AddressSpaceFallbackReason::None,
                1usize << 52,
                2,
                1,
                AddressSpaceFallbackReason::None,
            ),
            TlbSwitchDecision::Flush(AsidSwitchFallbackReason::InvalidWidth)
        );
    }
}
