//! The EEVDF tuning profile.
//!
//! There is no mutable or task-local policy state: the profile is a plain
//! constant consumed by the model, and it holds the original EEVDF constants.

/// The small, auditable set of EEVDF constraint parameters selected for one
/// build.
///
/// Target and sleeper values are expressed in scheduler ticks.  The model
/// converts the sleeper values to its fixed-point virtual-time units.  All
/// five values are powers of two so a profile can be reconstructed from a
/// read-only diagnostic snapshot without hidden tuning state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EevdfProfile {
    /// Stable profile name exported by the diagnostics/API boundary.
    pub name: &'static str,
    /// Normal-class request target in scheduler ticks.
    pub normal_target_ticks: u128,
    /// Batch-class request target in scheduler ticks.
    pub batch_target_ticks: u128,
    /// Idle-class request target in scheduler ticks.
    pub idle_target_ticks: u128,
    /// Sleeper grace interval in scheduler ticks.
    pub sleeper_grace_ticks: u128,
    /// Sleeper decay window in scheduler ticks.
    pub sleeper_decay_ticks: u128,
}

impl EevdfProfile {
    /// Returns whether every tunable value is a positive power of two.
    pub const fn has_power_of_two_parameters(self) -> bool {
        is_power_of_two(self.normal_target_ticks)
            && is_power_of_two(self.batch_target_ticks)
            && is_power_of_two(self.idle_target_ticks)
            && is_power_of_two(self.sleeper_grace_ticks)
            && is_power_of_two(self.sleeper_decay_ticks)
    }
}

const fn is_power_of_two(value: u128) -> bool {
    value != 0 && value & value.wrapping_sub(1) == 0
}

// The balanced profile is the historical EEVDF configuration, so the scheduler's
// ordinary hot path is independent of any profile selection.
pub const EEVDF_PROFILE: EevdfProfile = EevdfProfile {
    name: "balanced",
    normal_target_ticks: 8,
    batch_target_ticks: 32,
    idle_target_ticks: 8,
    sleeper_grace_ticks: 8,
    sleeper_decay_ticks: 64,
};

/// Return the immutable profile for API and diagnostic consumers.
pub const fn eevdf_profile() -> EevdfProfile {
    EEVDF_PROFILE
}

#[cfg(test)]
mod tests {
    use super::EEVDF_PROFILE;

    fn profile_constraints_hold(profile: super::EevdfProfile) -> bool {
        profile.normal_target_ticks <= profile.batch_target_ticks
            && profile.sleeper_grace_ticks <= profile.sleeper_decay_ticks
    }

    #[test]
    fn selected_profile_is_reconstructible_and_power_of_two() {
        assert!(EEVDF_PROFILE.has_power_of_two_parameters());
        assert!(profile_constraints_hold(EEVDF_PROFILE));
    }

    #[test]
    fn balanced_profile_keeps_the_original_constants() {
        assert_eq!(EEVDF_PROFILE.name, "balanced");
        assert_eq!(EEVDF_PROFILE.normal_target_ticks, 8);
        assert_eq!(EEVDF_PROFILE.batch_target_ticks, 32);
        assert_eq!(EEVDF_PROFILE.idle_target_ticks, 8);
        assert_eq!(EEVDF_PROFILE.sleeper_grace_ticks, 8);
        assert_eq!(EEVDF_PROFILE.sleeper_decay_ticks, 64);
    }
}
