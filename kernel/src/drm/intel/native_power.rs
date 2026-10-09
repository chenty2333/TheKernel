//! Native commit-tail power callbacks backed by the Intel display power map.
//!
//! This adapter owns no global state or lock. The caller borrows the existing
//! [`PowerState`] for the full callback sequence, which keeps domain reference
//! updates serialized with the display transaction and prevents a callback
//! from succeeding after its register/power owner has gone away.

use alloc::vec::Vec;

use intel_display::power_map::PowerDomain;

use super::{PowerError, PowerState};
use crate::drm::intel::regs::Registers;

/// Power and DC-state operations needed around native modeset programming.
///
/// Domain methods preserve the map's checked reference counts and HSW/TC well
/// sequencing. DC exit is synchronous (the source write/readback must finish
/// before MMIO programming); DC entry may return `false` when the target was
/// recorded but DMC firmware is not loaded, in which case DC5/DC6 stays off.
pub(crate) trait NativePowerOps {
    /// Acquire a translated display power-domain reference, equivalent to
    /// `intel_display_power_get()`.
    fn power_domain_get(&mut self, domain: PowerDomain) -> Result<(), PowerError>;

    /// Release a paired domain reference, equivalent to
    /// `intel_display_power_put()`.
    fn power_domain_put(&mut self, domain: PowerDomain) -> Result<(), PowerError>;

    /// Exit DC5/DC6 before changing display state. The translated
    /// `gen9_set_dc_state(DC_STATE_DISABLE)` write is synchronous.
    fn dc_state_exit(&mut self) -> Result<(), PowerError>;

    /// Enter the current sanitized DC5/DC6 target after the modeset. Returns
    /// `false` if DMC firmware is not loaded and the hardware must stay in DC
    /// disabled state.
    fn dc_state_enter(&mut self) -> Result<bool, PowerError>;

    /// Change the desired idle state using the translated DC-off-well cycle.
    /// The actual state can remain disabled while that well is referenced.
    fn set_dc_state_target(&mut self, requested: u32) -> Result<bool, PowerError>;
}

/// A commit-tail's exact power-domain references. Domains are acquired in
/// caller-provided dependency order and released in reverse order. DC states
/// are disabled before the first reference is acquired and only reconsidered
/// after every reference has been released.
///
/// This is explicit rather than `Drop`-based: power-domain put performs MMIO
/// and can fail, so silently ignoring a destructor error would corrupt the
/// map-backed reference counts. The commit-tail must call `release` on every
/// path after `acquire` succeeds.
#[derive(Debug)]
pub(crate) struct NativePowerLease {
    domains: Vec<PowerDomain>,
}

#[derive(Debug)]
pub(crate) struct NativePowerFailure {
    pub(crate) cause: PowerError,
    pub(crate) unwind: Vec<PowerError>,
}

impl NativePowerLease {
    pub(crate) fn acquire(
        ops: &mut impl NativePowerOps,
        domains: &[PowerDomain],
    ) -> Result<Self, NativePowerFailure> {
        ops.dc_state_exit().map_err(|cause| NativePowerFailure {
            cause,
            unwind: Vec::new(),
        })?;

        let mut acquired = Vec::with_capacity(domains.len());
        for &domain in domains {
            if let Err(cause) = ops.power_domain_get(domain) {
                let mut unwind = Vec::new();
                for held in acquired.into_iter().rev() {
                    if let Err(error) = ops.power_domain_put(held) {
                        unwind.push(error);
                    }
                }
                // DC entry is not attempted after a failed acquisition: the
                // transaction did not reach a stable post-commit boundary.
                return Err(NativePowerFailure { cause, unwind });
            }
            acquired.push(domain);
        }
        Ok(Self { domains: acquired })
    }

    /// Release all owned references in reverse dependency order, continuing
    /// after errors so one failed put cannot strand later references. DC
    /// entry is attempted only when all puts succeeded; `false` is the
    /// supported firmware-unavailable result and is not an error.
    pub(crate) fn release(self, ops: &mut impl NativePowerOps) -> Result<bool, NativePowerFailure> {
        let mut errors = Vec::new();
        for domain in self.domains.into_iter().rev() {
            if let Err(error) = ops.power_domain_put(domain) {
                errors.push(error);
            }
        }
        if let Some(cause) = errors.first().cloned() {
            return Err(NativePowerFailure {
                cause,
                unwind: errors.into_iter().skip(1).collect(),
            });
        }
        ops.dc_state_enter().map_err(|cause| NativePowerFailure {
            cause,
            unwind: Vec::new(),
        })
    }
}

/// Per-transaction bridge to the kernel-owned, map-backed power references.
pub(crate) struct NativePowerAdapter<'a, R: Registers> {
    power: &'a mut PowerState,
    registers: &'a R,
}

impl<'a, R: Registers> NativePowerAdapter<'a, R> {
    pub(crate) fn new(power: &'a mut PowerState, registers: &'a R) -> Self {
        Self { power, registers }
    }
}

impl<R: Registers> NativePowerOps for NativePowerAdapter<'_, R> {
    fn power_domain_get(&mut self, domain: PowerDomain) -> Result<(), PowerError> {
        self.power.get_domain(self.registers, domain)
    }

    fn power_domain_put(&mut self, domain: PowerDomain) -> Result<(), PowerError> {
        self.power.put_domain(self.registers, domain)
    }

    fn dc_state_exit(&mut self) -> Result<(), PowerError> {
        self.power.exit_dc_states(self.registers)
    }

    fn dc_state_enter(&mut self) -> Result<bool, PowerError> {
        self.power.enter_dc_states(self.registers)
    }

    fn set_dc_state_target(&mut self, requested: u32) -> Result<bool, PowerError> {
        self.power.set_target_dc_state(self.registers, requested)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[derive(Default)]
    struct FakePower {
        events: Vec<(u8, PowerDomain)>,
        fail_get: Option<PowerDomain>,
        fail_put: Option<PowerDomain>,
        dc_enter: bool,
    }

    impl NativePowerOps for FakePower {
        fn power_domain_get(&mut self, domain: PowerDomain) -> Result<(), PowerError> {
            self.events.push((1, domain));
            if self.fail_get == Some(domain) {
                Err(PowerError::PowerDomain("get failed".into()))
            } else {
                Ok(())
            }
        }

        fn power_domain_put(&mut self, domain: PowerDomain) -> Result<(), PowerError> {
            self.events.push((2, domain));
            if self.fail_put == Some(domain) {
                Err(PowerError::PowerDomain("put failed".into()))
            } else {
                Ok(())
            }
        }

        fn dc_state_exit(&mut self) -> Result<(), PowerError> {
            self.events.push((3, PowerDomain::DcOff));
            Ok(())
        }

        fn dc_state_enter(&mut self) -> Result<bool, PowerError> {
            self.events.push((4, PowerDomain::DcOff));
            Ok(self.dc_enter)
        }

        fn set_dc_state_target(&mut self, _: u32) -> Result<bool, PowerError> {
            Ok(true)
        }
    }

    #[test]
    fn lease_brackets_domains_and_releases_in_reverse_order() {
        let mut power = FakePower {
            dc_enter: true,
            ..FakePower::default()
        };
        let lease =
            NativePowerLease::acquire(&mut power, &[PowerDomain::PipeA, PowerDomain::TranscoderA])
                .unwrap();
        assert!(lease.release(&mut power).unwrap());
        assert_eq!(
            power.events,
            vec![
                (3, PowerDomain::DcOff),
                (1, PowerDomain::PipeA),
                (1, PowerDomain::TranscoderA),
                (2, PowerDomain::TranscoderA),
                (2, PowerDomain::PipeA),
                (4, PowerDomain::DcOff),
            ]
        );
    }

    #[test]
    fn failed_acquire_unwinds_prior_domains_without_entering_dc() {
        let mut power = FakePower {
            fail_get: Some(PowerDomain::TranscoderA),
            ..FakePower::default()
        };
        let failure =
            NativePowerLease::acquire(&mut power, &[PowerDomain::PipeA, PowerDomain::TranscoderA])
                .unwrap_err();
        assert!(failure.unwind.is_empty());
        assert_eq!(
            power.events,
            vec![
                (3, PowerDomain::DcOff),
                (1, PowerDomain::PipeA),
                (1, PowerDomain::TranscoderA),
                (2, PowerDomain::PipeA),
            ]
        );
    }

    #[test]
    fn failed_put_does_not_reenter_dc_and_still_releases_other_domains() {
        let mut power = FakePower {
            fail_put: Some(PowerDomain::TranscoderA),
            ..FakePower::default()
        };
        let lease =
            NativePowerLease::acquire(&mut power, &[PowerDomain::PipeA, PowerDomain::TranscoderA])
                .unwrap();
        let failure = lease.release(&mut power).unwrap_err();
        assert!(failure.unwind.is_empty());
        assert_eq!(
            power.events,
            vec![
                (3, PowerDomain::DcOff),
                (1, PowerDomain::PipeA),
                (1, PowerDomain::TranscoderA),
                (2, PowerDomain::TranscoderA),
                (2, PowerDomain::PipeA),
            ]
        );
    }
}
