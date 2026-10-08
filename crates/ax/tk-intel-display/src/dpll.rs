// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_dpll.c.
// Copyright © 2020 Intel Corporation.
// MIT permission text: LICENSE-MIT.
//! CRTC-side clock dispatch and shared-DPLL state preparation.

use crate::{Error, dpll_mgr::IclDpllHwState};

/// The DPLL-related subset of one CRTC atomic state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpllCrtcState {
    pub needs_modeset: bool,
    pub hw_enabled: bool,
    pub dpll_reserved: bool,
    pub hw_state: IclDpllHwState,
}

/// The source considers clocks equal when they differ by at most one kHz.
// upstream: intel_dpll.c intel_dpll_clock_matches()
pub const fn intel_dpll_clock_matches(clock1_khz: u32, clock2_khz: u32) -> bool {
    clock1_khz.abs_diff(clock2_khz) <= 1
}

/// Clear stale divider state, skip disabled CRTCs, then dispatch clock calc.
// upstream: intel_dpll.c intel_dpll_crtc_compute_clock()
pub fn intel_dpll_crtc_compute_clock(
    state: &mut DpllCrtcState,
    compute: impl FnOnce() -> Result<IclDpllHwState, Error>,
) -> Result<(), Error> {
    if !state.needs_modeset {
        return Err(Error::Refused);
    }
    state.hw_state = IclDpllHwState::default();
    if !state.hw_enabled {
        return Ok(());
    }
    state.hw_state = compute()?;
    Ok(())
}

/// Reserve a DPLL only for an enabled CRTC that does not already own one.
// upstream: intel_dpll.c intel_dpll_crtc_get_dpll()
pub fn intel_dpll_crtc_get_dpll(
    state: &DpllCrtcState,
    reserve: impl FnOnce() -> Result<(), Error>,
) -> Result<(), Error> {
    if !state.needs_modeset {
        return Err(Error::Refused);
    }
    if !state.hw_enabled || state.dpll_reserved {
        return Ok(());
    }
    reserve()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_match_has_the_upstream_one_khz_tolerance() {
        assert!(intel_dpll_clock_matches(148_500, 148_501));
        assert!(intel_dpll_clock_matches(148_500, 148_499));
        assert!(!intel_dpll_clock_matches(148_500, 148_502));
    }

    #[test]
    fn modeset_compute_clears_stale_state_and_skips_disabled_crtcs() {
        let mut state = DpllCrtcState {
            needs_modeset: true,
            hw_enabled: false,
            hw_state: IclDpllHwState {
                cfgcr0: 1,
                ..IclDpllHwState::default()
            },
            ..DpllCrtcState::default()
        };
        let mut called = false;
        intel_dpll_crtc_compute_clock(&mut state, || {
            called = true;
            Ok(IclDpllHwState::default())
        })
        .unwrap();
        assert!(!called);
        assert_eq!(state.hw_state, IclDpllHwState::default());
        state.hw_enabled = true;
        intel_dpll_crtc_compute_clock(&mut state, || {
            Ok(IclDpllHwState {
                cfgcr1: 7,
                ..IclDpllHwState::default()
            })
        })
        .unwrap();
        assert_eq!(state.hw_state.cfgcr1, 7);
        state.needs_modeset = false;
        assert_eq!(
            intel_dpll_crtc_compute_clock(&mut state, || Ok(IclDpllHwState::default())),
            Err(Error::Refused)
        );
    }

    #[test]
    fn reservation_dispatch_matches_enabled_and_existing_state_guards() {
        let mut calls = 0;
        let state = DpllCrtcState {
            needs_modeset: true,
            hw_enabled: false,
            ..DpllCrtcState::default()
        };
        intel_dpll_crtc_get_dpll(&state, || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        let state = DpllCrtcState {
            dpll_reserved: true,
            hw_enabled: true,
            ..state
        };
        intel_dpll_crtc_get_dpll(&state, || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 0);
        let state = DpllCrtcState {
            dpll_reserved: false,
            ..state
        };
        intel_dpll_crtc_get_dpll(&state, || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 1);
    }
}
