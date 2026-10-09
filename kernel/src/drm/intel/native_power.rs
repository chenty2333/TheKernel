//! Native commit-tail power callbacks backed by the Intel display power map.
//!
//! This adapter owns no global state or lock. The caller borrows the existing
//! [`PowerState`] for the full callback sequence, which keeps domain reference
//! updates serialized with the display transaction and prevents a callback
//! from succeeding after its register/power owner has gone away.

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
