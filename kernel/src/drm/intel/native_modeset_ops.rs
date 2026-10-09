//! Hardware callback contract for the translated Native atomic commit tail.
//!
//! This is intentionally only the boundary, not a backend implementation:
//! the opt-in selector must continue to fail closed until every callback is
//! backed by checked MMIO, power ownership, readback, and rollback.  Keeping
//! the callbacks explicit prevents a generic commit-tail action from being
//! reported as successful by an empty hook.

use intel_display::intel_display_modeset_full::{
    EncoderTransition, PipeState, PipeTransition, PlaneTransition,
};

/// Native hardware operations required by the display-12/13 atomic commit
/// sequence. Implementations must return an error on unsupported state and
/// must not use a successful no-op as a placeholder.
pub(super) trait NativeCdclkOps {
    type Error;

    /// Pre-plane CDCLK transition (`intel_cdclk_set_cdclk()`), including the
    /// required PCode and peripheral-ordering hooks.
    fn set_cdclk_pre_plane(&mut self, target_khz: u32) -> Result<(), Self::Error>;

    /// Post-plane CDCLK transition/readback (`intel_cdclk_set_cdclk()`).
    fn set_cdclk_post_plane(&mut self, target_khz: u32) -> Result<(), Self::Error>;
}

/// Shared-PLL callbacks use the source manager's typed atomic/CRTC/encoder
/// state. A DRM projection alone is not enough to synthesize this state.
pub(super) trait NativeDpllOps {
    /// Compute/release/reserve and swap the shared PLL state
    /// (`intel_dpll_compute()` / `intel_dpll_reserve()` / `intel_dpll_swap_state()`).
    fn dpll_get(
        &mut self,
        atomic: &mut intel_display::intel_dpll_mgr_full::IntelAtomicState,
        crtc: &intel_display::intel_dpll_mgr_full::IntelCrtc,
        encoder: &intel_display::intel_dpll_mgr_full::IntelEncoder,
    ) -> Result<(), super::shared_dpll::DpllFailure>;

    /// Enable the reserved PLL (`intel_enable_shared_dpll()`).
    fn dpll_enable(
        &mut self,
        state: &intel_display::intel_dpll_mgr_full::CrtcState,
    ) -> Result<(), super::shared_dpll::DpllFailure>;

    /// Disable/release an old PLL (`intel_disable_shared_dpll()`).
    fn dpll_disable(
        &mut self,
        state: &intel_display::intel_dpll_mgr_full::CrtcState,
    ) -> Result<(), super::shared_dpll::DpllFailure>;
}

pub(super) trait NativeModesetOps: NativeCdclkOps + NativeDpllOps {
    /// CRTC enable phase (`hsw_crtc_enable()` / `skl_commit_modeset_enables()`).
    fn crtc_enable(&mut self, state: &PipeState) -> Result<(), Self::Error>;

    /// CRTC disable phase (`intel_crtc_disable()` / `intel_commit_modeset_disables()`).
    fn crtc_disable(&mut self, state: &PipeState) -> Result<(), Self::Error>;

    /// Encoder pre-enable (`intel_encoders_pre_enable()`).
    fn encoder_pre_enable(&mut self, encoder: &EncoderTransition) -> Result<(), Self::Error>;

    /// Encoder enable (`intel_encoders_enable()`).
    fn encoder_enable(&mut self, encoder: &EncoderTransition) -> Result<(), Self::Error>;

    /// Encoder disable (`intel_encoders_disable()`).
    fn encoder_disable(&mut self, encoder: &EncoderTransition) -> Result<(), Self::Error>;

    /// Encoder post-disable (`intel_encoders_post_disable()`).
    fn encoder_post_disable(&mut self, encoder: &EncoderTransition) -> Result<(), Self::Error>;

    /// Pipe configuration/update (`intel_crtc_enable()` and
    /// `intel_crtc_update_noarm()` ordering in `intel_display.c`).
    fn update_pipe(&mut self, transition: &PipeTransition) -> Result<(), Self::Error>;

    /// Universal-plane update (`skl_universal_plane` update/arm path).
    fn update_plane(&mut self, plane: &PlaneTransition) -> Result<(), Self::Error>;

    /// Universal-plane disable (`skl_universal_plane_disable_arm()` path).
    fn disable_plane(&mut self, plane: &PlaneTransition) -> Result<(), Self::Error>;

    /// Acquire a map-backed display power-domain reference (`intel_display_power_get()`).
    fn power_domain_get(&mut self, domain: u8) -> Result<(), Self::Error>;

    /// Release a previously acquired display power-domain reference
    /// (`intel_display_power_put()`).
    fn power_domain_put(&mut self, domain: u8) -> Result<(), Self::Error>;
}
