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
    pub output_dsi: bool,
    pub has_pch_encoder: bool,
    pub dotclock_khz: u32,
    pub adjusted_mode_crtc_clock_khz: u32,
}

/// Clock callback family selected by `intel_dpll_init_clock_hook()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DpllClockHooks {
    IclHsw,
    Dg2,
    MeteorLake,
    Xe3,
    Legacy,
}

/// Select the source's DPLL CRTC hook family, in its platform precedence order.
// upstream: intel_dpll.c intel_dpll_init_clock_hook()
pub const fn intel_dpll_init_clock_hook(display_version: u8, dg2: bool) -> DpllClockHooks {
    if display_version >= 35 {
        DpllClockHooks::Xe3
    } else if display_version >= 14 {
        DpllClockHooks::MeteorLake
    } else if dg2 {
        DpllClockHooks::Dg2
    } else if display_version >= 9 {
        DpllClockHooks::IclHsw
    } else {
        DpllClockHooks::Legacy
    }
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

/// HSW+ CRTC clock calculation dispatch; display-12/13 reaches the shared
/// DPLL manager through `compute_dpll` and then refreshes the adjusted dotclock.
// upstream: intel_dpll.c hsw_crtc_compute_clock()
pub fn hsw_crtc_compute_clock(
    display_version: u8,
    state: &mut DpllCrtcState,
    compute_dpll: impl FnOnce() -> Result<(), Error>,
) -> Result<(), Error> {
    if display_version < 11 && state.output_dsi {
        return Ok(());
    }
    compute_dpll()?;
    if state.output_dsi {
        return Ok(());
    }
    if !state.has_pch_encoder {
        state.adjusted_mode_crtc_clock_khz = state.dotclock_khz;
    }
    Ok(())
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
    fn display12_and_13_select_the_hsw_shared_dpll_hooks() {
        for display_version in [12, 13] {
            assert_eq!(
                intel_dpll_init_clock_hook(display_version, false),
                DpllClockHooks::IclHsw
            );
        }
        assert_eq!(intel_dpll_init_clock_hook(12, true), DpllClockHooks::Dg2);
        assert_eq!(
            intel_dpll_init_clock_hook(14, false),
            DpllClockHooks::MeteorLake
        );
        assert_eq!(intel_dpll_init_clock_hook(35, false), DpllClockHooks::Xe3);
    }

    #[test]
    fn hsw_clock_callback_runs_shared_calc_then_updates_only_non_pch_dotclock() {
        let mut state = DpllCrtcState {
            dotclock_khz: 148_500,
            has_pch_encoder: false,
            ..DpllCrtcState::default()
        };
        let mut calls = 0;
        hsw_crtc_compute_clock(13, &mut state, || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(state.adjusted_mode_crtc_clock_khz, 148_500);

        state.has_pch_encoder = true;
        state.adjusted_mode_crtc_clock_khz = 0;
        hsw_crtc_compute_clock(13, &mut state, || Ok(())).unwrap();
        assert_eq!(state.adjusted_mode_crtc_clock_khz, 0);

        state.output_dsi = true;
        let mut old_dsi_calls = 0;
        hsw_crtc_compute_clock(10, &mut state, || {
            old_dsi_calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(old_dsi_calls, 0);
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
