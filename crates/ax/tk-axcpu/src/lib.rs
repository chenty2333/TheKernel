#![cfg_attr(not(test), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
#![doc = include_str!("../README.md")]

#[macro_use]
extern crate log;

#[macro_use]
extern crate memory_addr;

#[cfg(feature = "asid-switch-diagnostics")]
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

/// Approximate snapshot of default-off ASID switch diagnostics.
#[cfg(feature = "asid-switch-diagnostics")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsidSwitchDiagnosticsSnapshot {
    enabled: bool,
    fast_path_avoided: usize,
    fallback_asid_zero: usize,
    fallback_invalid_width: usize,
    fallback_exhausted: usize,
    fallback_generation_mismatch: usize,
    fallback_same_id_different_root: usize,
    saturated: bool,
}

#[cfg(feature = "asid-switch-diagnostics")]
impl AsidSwitchDiagnosticsSnapshot {
    /// Returns whether future switch decisions are being recorded.
    pub const fn enabled(self) -> bool {
        self.enabled
    }

    /// Returns full local TLB flushes avoided by the safe fast path.
    pub const fn fast_path_avoided(self) -> usize {
        self.fast_path_avoided
    }

    /// Returns fallbacks caused by a deliberate ASID-0 identity.
    pub const fn fallback_asid_zero(self) -> usize {
        self.fallback_asid_zero
    }

    /// Returns fallbacks caused by an invalid hardware ASID width.
    pub const fn fallback_invalid_width(self) -> usize {
        self.fallback_invalid_width
    }

    /// Returns fallbacks caused by allocator exhaustion.
    pub const fn fallback_exhausted(self) -> usize {
        self.fallback_exhausted
    }

    /// Returns fallbacks caused by a generation mismatch.
    pub const fn fallback_generation_mismatch(self) -> usize {
        self.fallback_generation_mismatch
    }

    /// Returns fallbacks caused by one ASID naming different roots.
    pub const fn fallback_same_id_different_root(self) -> usize {
        self.fallback_same_id_different_root
    }

    /// Returns whether any enabled counter saturated at `usize::MAX`.
    pub const fn saturated(self) -> bool {
        self.saturated
    }
}

#[cfg(feature = "asid-switch-diagnostics")]
struct AsidSwitchDiagnostics {
    fast_path: axpmu::SoftwareDiagnostics,
    fallback_asid_zero: AtomicUsize,
    fallback_invalid_width: AtomicUsize,
    fallback_exhausted: AtomicUsize,
    fallback_generation_mismatch: AtomicUsize,
    fallback_same_id_different_root: AtomicUsize,
    saturated: AtomicBool,
}

#[cfg(feature = "asid-switch-diagnostics")]
impl AsidSwitchDiagnostics {
    const fn new() -> Self {
        Self {
            fast_path: axpmu::SoftwareDiagnostics::new(),
            fallback_asid_zero: AtomicUsize::new(0),
            fallback_invalid_width: AtomicUsize::new(0),
            fallback_exhausted: AtomicUsize::new(0),
            fallback_generation_mismatch: AtomicUsize::new(0),
            fallback_same_id_different_root: AtomicUsize::new(0),
            saturated: AtomicBool::new(false),
        }
    }

    fn increment(&self, counter: &AtomicUsize) {
        if counter
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .is_err()
        {
            self.saturated.store(true, Ordering::Relaxed);
        }
    }

    #[inline]
    fn record(&self, decision: TlbSwitchDecision) {
        match decision {
            TlbSwitchDecision::Retain => self.fast_path.record_asid_tlb_flush_avoided(),
            TlbSwitchDecision::Flush(reason) => {
                if !self.fast_path.is_enabled() {
                    return;
                }
                let counter = match reason {
                    AsidSwitchFallbackReason::AsidZero => &self.fallback_asid_zero,
                    AsidSwitchFallbackReason::InvalidWidth => &self.fallback_invalid_width,
                    AsidSwitchFallbackReason::Exhausted => &self.fallback_exhausted,
                    AsidSwitchFallbackReason::GenerationMismatch => {
                        &self.fallback_generation_mismatch
                    }
                    AsidSwitchFallbackReason::SameIdDifferentRoot => {
                        &self.fallback_same_id_different_root
                    }
                };
                self.increment(counter);
            }
        }
    }

    fn snapshot(&self) -> AsidSwitchDiagnosticsSnapshot {
        let fast_path = self.fast_path.snapshot();
        AsidSwitchDiagnosticsSnapshot {
            enabled: self.fast_path.is_enabled(),
            fast_path_avoided: fast_path.asid_tlb_flushes_avoided(),
            fallback_asid_zero: self.fallback_asid_zero.load(Ordering::Relaxed),
            fallback_invalid_width: self.fallback_invalid_width.load(Ordering::Relaxed),
            fallback_exhausted: self.fallback_exhausted.load(Ordering::Relaxed),
            fallback_generation_mismatch: self.fallback_generation_mismatch.load(Ordering::Relaxed),
            fallback_same_id_different_root: self
                .fallback_same_id_different_root
                .load(Ordering::Relaxed),
            saturated: fast_path.is_saturated() || self.saturated.load(Ordering::Relaxed),
        }
    }

    fn reset(&self) {
        let _ = self.fast_path.snapshot_and_reset();
        self.fallback_asid_zero.store(0, Ordering::Relaxed);
        self.fallback_invalid_width.store(0, Ordering::Relaxed);
        self.fallback_exhausted.store(0, Ordering::Relaxed);
        self.fallback_generation_mismatch
            .store(0, Ordering::Relaxed);
        self.fallback_same_id_different_root
            .store(0, Ordering::Relaxed);
        self.saturated.store(false, Ordering::Relaxed);
    }
}

#[cfg(feature = "asid-switch-diagnostics")]
static ASID_SWITCH_DIAGNOSTICS: AsidSwitchDiagnostics = AsidSwitchDiagnostics::new();

/// Enables or disables future ASID switch diagnostic increments.
#[cfg(feature = "asid-switch-diagnostics")]
pub fn set_asid_switch_diagnostics_enabled(enabled: bool) -> bool {
    ASID_SWITCH_DIAGNOSTICS.fast_path.set_enabled(enabled)
}

/// Returns an approximate lock-free ASID switch diagnostic snapshot.
#[cfg(feature = "asid-switch-diagnostics")]
pub fn asid_switch_diagnostics_snapshot() -> AsidSwitchDiagnosticsSnapshot {
    ASID_SWITCH_DIAGNOSTICS.snapshot()
}

/// Resets ASID switch diagnostic counters without enabling collection.
#[cfg(feature = "asid-switch-diagnostics")]
pub fn reset_asid_switch_diagnostics() {
    ASID_SWITCH_DIAGNOSTICS.reset();
}

#[cfg(feature = "asid-switch-diagnostics")]
#[inline]
fn record_asid_switch_decision(decision: TlbSwitchDecision) {
    ASID_SWITCH_DIAGNOSTICS.record(decision);
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

    #[cfg(feature = "asid-switch-diagnostics")]
    #[test]
    fn diagnostics_are_default_off_and_classify_each_decision() {
        let diagnostics = AsidSwitchDiagnostics::new();
        diagnostics.record(TlbSwitchDecision::Retain);
        assert_eq!(diagnostics.snapshot().fast_path_avoided(), 0);

        assert!(!diagnostics.fast_path.set_enabled(true));
        diagnostics.record(TlbSwitchDecision::Retain);
        for reason in [
            AsidSwitchFallbackReason::AsidZero,
            AsidSwitchFallbackReason::InvalidWidth,
            AsidSwitchFallbackReason::Exhausted,
            AsidSwitchFallbackReason::GenerationMismatch,
            AsidSwitchFallbackReason::SameIdDifferentRoot,
        ] {
            diagnostics.record(TlbSwitchDecision::Flush(reason));
        }
        let snapshot = diagnostics.snapshot();
        assert!(snapshot.enabled());
        assert_eq!(snapshot.fast_path_avoided(), 1);
        assert_eq!(snapshot.fallback_asid_zero(), 1);
        assert_eq!(snapshot.fallback_invalid_width(), 1);
        assert_eq!(snapshot.fallback_exhausted(), 1);
        assert_eq!(snapshot.fallback_generation_mismatch(), 1);
        assert_eq!(snapshot.fallback_same_id_different_root(), 1);
        assert!(!snapshot.saturated());

        diagnostics.reset();
        assert_eq!(diagnostics.snapshot().fast_path_avoided(), 0);
    }

    #[cfg(feature = "asid-switch-diagnostics")]
    #[test]
    fn global_diagnostic_control_records_the_switch_boundary() {
        set_asid_switch_diagnostics_enabled(false);
        reset_asid_switch_diagnostics();
        record_asid_switch_decision(TlbSwitchDecision::Retain);
        assert_eq!(asid_switch_diagnostics_snapshot().fast_path_avoided(), 0);

        set_asid_switch_diagnostics_enabled(true);
        record_asid_switch_decision(TlbSwitchDecision::Retain);
        set_asid_switch_diagnostics_enabled(false);
        assert_eq!(asid_switch_diagnostics_snapshot().fast_path_avoided(), 1);
        reset_asid_switch_diagnostics();
    }
}
