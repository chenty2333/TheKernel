// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_cdclk.c:
// adlp_cdclk_table, _intel_pixel_rate_to_cdclk, bxt_calc_cdclk (table selection),
// intel_cdclk_can_{crawl,crawl_and_squash,squash,cd2x_update},
// cdclk_compute_crawl_and_squash_midpoint.
// Copyright © 2006-2017 Intel Corporation. MIT permission text: ../LICENSE-MIT.
// ADL-P B0+ / ADL-N D0 only; A-step, other platforms, PCODE voltage, PLL crawl,
// CDCLK MMIO and per-plane/bandwidth/audio requirement calculation omitted.
use crate::{Error, device::Step};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CdclkValue {
    pub refclk_khz: u32,
    pub cdclk_khz: u32,
    pub ratio: u32,
}
pub const ADLP_CDCLK_TABLE: [CdclkValue; 15] = [
    CdclkValue {
        refclk_khz: 19200,
        cdclk_khz: 172800,
        ratio: 27,
    },
    CdclkValue {
        refclk_khz: 19200,
        cdclk_khz: 192000,
        ratio: 20,
    },
    CdclkValue {
        refclk_khz: 19200,
        cdclk_khz: 307200,
        ratio: 32,
    },
    CdclkValue {
        refclk_khz: 19200,
        cdclk_khz: 556800,
        ratio: 58,
    },
    CdclkValue {
        refclk_khz: 19200,
        cdclk_khz: 652800,
        ratio: 68,
    },
    CdclkValue {
        refclk_khz: 24000,
        cdclk_khz: 176000,
        ratio: 22,
    },
    CdclkValue {
        refclk_khz: 24000,
        cdclk_khz: 192000,
        ratio: 16,
    },
    CdclkValue {
        refclk_khz: 24000,
        cdclk_khz: 312000,
        ratio: 26,
    },
    CdclkValue {
        refclk_khz: 24000,
        cdclk_khz: 552000,
        ratio: 46,
    },
    CdclkValue {
        refclk_khz: 24000,
        cdclk_khz: 648000,
        ratio: 54,
    },
    CdclkValue {
        refclk_khz: 38400,
        cdclk_khz: 179200,
        ratio: 14,
    },
    CdclkValue {
        refclk_khz: 38400,
        cdclk_khz: 192000,
        ratio: 10,
    },
    CdclkValue {
        refclk_khz: 38400,
        cdclk_khz: 307200,
        ratio: 16,
    },
    CdclkValue {
        refclk_khz: 38400,
        cdclk_khz: 556800,
        ratio: 29,
    },
    CdclkValue {
        refclk_khz: 38400,
        cdclk_khz: 652800,
        ratio: 34,
    },
];
/// Display 13 has PPC=2 and guardband=100. This is only the pixel-rate
/// requirement, NOT intel_crtc_min_cdclk (plane/bandwidth/audio also matter).
pub fn pixel_rate_min_cdclk(pixel_rate_khz: u32) -> u32 {
    pixel_rate_khz.div_ceil(2)
}
/// Caller supplies the already-computed GLOBAL minimum and fuse maximum. No
/// guessed 1080p watermark/DBUF/audio constraint. Unlike C's warning+max return,
/// an unsatisfiable request is an error, never an apparently valid frequency.
pub fn bxt_calc_cdclk(
    step: Step,
    refclk_khz: u32,
    minimum_khz: u32,
    maximum_khz: u32,
) -> Result<CdclkValue, Error> {
    if !matches!(step, Step::B0 | Step::C0 | Step::D0) {
        return Err(Error::Refused);
    }
    ADLP_CDCLK_TABLE
        .into_iter()
        .find(|v| {
            v.refclk_khz == refclk_khz && v.cdclk_khz >= minimum_khz && v.cdclk_khz <= maximum_khz
        })
        .ok_or(Error::Refused)
}

/// CDCLK's logical/actual frequency state used by the transition predicates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CdclkConfig {
    pub cdclk_khz: u32,
    pub vco_khz: u32,
    pub refclk_khz: u32,
    pub waveform: u16,
}

/// Display capabilities that select the source's in-place frequency update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CdclkTransitionCaps {
    pub display_version: u8,
    pub has_crawl: bool,
    pub has_squash: bool,
}

/// A CDCLK update that can be made without disabling the complete PLL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CdclkTransition {
    Unchanged,
    FullPll,
    Crawl,
    Squash,
    Cd2xDivider,
    CrawlAndSquash(CdclkConfig),
}

const SQUASH_WINDOW: u32 = 16;

const fn squash_divider(waveform: u16) -> u32 {
    if waveform == 0 {
        SQUASH_WINDOW
    } else {
        waveform.count_ones()
    }
}

fn cd2x_divider(config: CdclkConfig) -> Option<u32> {
    if config.cdclk_khz == 0 {
        return None;
    }
    Some(
        (config.vco_khz * squash_divider(config.waveform) + config.cdclk_khz * 8)
            / (config.cdclk_khz * SQUASH_WINDOW),
    )
}

fn pll_divider(config: CdclkConfig) -> Option<u32> {
    (config.cdclk_khz != 0).then(|| {
        ((u64::from(config.vco_khz) + u64::from(config.cdclk_khz) / 2)
            / u64::from(config.cdclk_khz)) as u32
    })
}

/// Check whether a frequency change needs reprogramming at all.
// upstream: intel_cdclk.c intel_cdclk_clock_changed()
pub const fn clock_changed(a: CdclkConfig, b: CdclkConfig) -> bool {
    a.cdclk_khz != b.cdclk_khz || a.vco_khz != b.vco_khz || a.refclk_khz != b.refclk_khz
}

/// Whether the source can smoothly crawl between the two PLL ratios.
// upstream: intel_cdclk.c intel_cdclk_can_crawl()
pub fn can_crawl(caps: CdclkTransitionCaps, a: CdclkConfig, b: CdclkConfig) -> bool {
    if !caps.has_crawl {
        return false;
    }
    pll_divider(a) == pll_divider(b)
        && a.vco_khz != 0
        && b.vco_khz != 0
        && a.vco_khz != b.vco_khz
        && a.refclk_khz == b.refclk_khz
}

/// Whether the waveform changes at a constant PLL VCO.
// upstream: intel_cdclk.c intel_cdclk_can_squash()
pub const fn can_squash(caps: CdclkTransitionCaps, a: CdclkConfig, b: CdclkConfig) -> bool {
    caps.has_squash
        && a.cdclk_khz != b.cdclk_khz
        && a.vco_khz != 0
        && a.vco_khz == b.vco_khz
        && a.refclk_khz == b.refclk_khz
}

/// Whether only the CD2X divider changes (platforms without a squasher).
// upstream: intel_cdclk.c intel_cdclk_can_cd2x_update()
pub const fn can_cd2x_update(caps: CdclkTransitionCaps, a: CdclkConfig, b: CdclkConfig) -> bool {
    caps.display_version >= 10
        && !caps.has_squash
        && a.cdclk_khz != b.cdclk_khz
        && a.vco_khz != 0
        && a.vco_khz == b.vco_khz
        && a.refclk_khz == b.refclk_khz
}

/// Compute the intermediate frequency for consecutive crawl and squash.
// upstream: intel_cdclk.c cdclk_compute_crawl_and_squash_midpoint()
pub fn crawl_and_squash_midpoint(
    caps: CdclkTransitionCaps,
    max_cdclk_khz: u32,
    old: CdclkConfig,
    new: CdclkConfig,
) -> Option<CdclkConfig> {
    if old.vco_khz == u32::MAX
        || !caps.has_crawl
        || !caps.has_squash
        || old.vco_khz == 0
        || new.vco_khz == 0
        || old.vco_khz == new.vco_khz
        || old.waveform == new.waveform
    {
        return None;
    }

    let old_div = cd2x_divider(old)?;
    let new_div = cd2x_divider(new)?;
    if old_div != new_div {
        return None;
    }

    let (vco_khz, waveform) = if squash_divider(new.waveform) > squash_divider(old.waveform) {
        (old.vco_khz, new.waveform)
    } else {
        (new.vco_khz, old.waveform)
    };
    let numerator = squash_divider(waveform) * vco_khz;
    let denominator = SQUASH_WINDOW * old_div;
    let cdclk_khz = (numerator + denominator / 2) / denominator;
    if cdclk_khz < old.cdclk_khz.min(new.cdclk_khz) || cdclk_khz > max_cdclk_khz {
        return None;
    }
    Some(CdclkConfig {
        cdclk_khz,
        vco_khz,
        refclk_khz: new.refclk_khz,
        waveform,
    })
}

/// Select the same update strategy as `_bxt_set_cdclk` for a known old state.
pub fn transition(
    caps: CdclkTransitionCaps,
    max_cdclk_khz: u32,
    old: CdclkConfig,
    new: CdclkConfig,
) -> CdclkTransition {
    if !clock_changed(old, new) {
        CdclkTransition::Unchanged
    } else if let Some(midpoint) = crawl_and_squash_midpoint(caps, max_cdclk_khz, old, new) {
        CdclkTransition::CrawlAndSquash(midpoint)
    } else if can_crawl(caps, old, new) {
        CdclkTransition::Crawl
    } else if can_squash(caps, old, new) {
        CdclkTransition::Squash
    } else if can_cd2x_update(caps, old, new) {
        CdclkTransition::Cd2xDivider
    } else {
        CdclkTransition::FullPll
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_192mhz_is_legal_but_pixel_rate_is_not_full_clock_policy() {
        assert_eq!(pixel_rate_min_cdclk(148500), 74250);
        for refclk in [19200, 24000, 38400] {
            assert_eq!(
                bxt_calc_cdclk(Step::D0, refclk, 192000, 652800)
                    .unwrap()
                    .cdclk_khz,
                192000
            );
        }
        assert_eq!(
            bxt_calc_cdclk(Step::D0, 38400, 74250, 652800)
                .unwrap()
                .cdclk_khz,
            179200
        );
        assert_eq!(pixel_rate_min_cdclk(u32::MAX), 2147483648);
    }
    #[test]
    fn unmet_limits_and_unported_steps_never_return_fuse_max_as_success() {
        for step in [Step::A0, Step::Future] {
            assert!(bxt_calc_cdclk(step, 38400, 74250, 652800).is_err());
        }
        assert!(bxt_calc_cdclk(Step::D0, 38400, 652801, 652800).is_err());
        assert!(bxt_calc_cdclk(Step::D0, 38400, 74250, 170000).is_err());
        assert!(bxt_calc_cdclk(Step::D0, 0, 74250, 652800).is_err());
    }

    #[test]
    fn transition_predicates_match_crawl_squash_and_cd2x_conditions() {
        let caps = CdclkTransitionCaps {
            display_version: 12,
            has_crawl: true,
            has_squash: true,
        };
        let a = CdclkConfig {
            cdclk_khz: 408000,
            vco_khz: 1224000,
            refclk_khz: 38400,
            waveform: 0xb6b6,
        };
        let crawl = CdclkConfig {
            cdclk_khz: 448800,
            vco_khz: 1346400,
            refclk_khz: 38400,
            waveform: a.waveform,
        };
        assert!(can_crawl(caps, a, crawl));
        assert_eq!(transition(caps, 800000, a, crawl), CdclkTransition::Crawl);

        let squash = CdclkConfig {
            cdclk_khz: 489600,
            vco_khz: a.vco_khz,
            refclk_khz: a.refclk_khz,
            waveform: 0xeeee,
        };
        assert!(can_squash(caps, a, squash));
        assert_eq!(transition(caps, 800000, a, squash), CdclkTransition::Squash);

        let divider_caps = CdclkTransitionCaps {
            has_squash: false,
            ..caps
        };
        let divider = CdclkConfig {
            cdclk_khz: 306000,
            ..a
        };
        assert!(can_cd2x_update(divider_caps, a, divider));
        assert_eq!(
            transition(divider_caps, 800000, a, divider),
            CdclkTransition::Cd2xDivider
        );
        assert_eq!(transition(caps, 800000, a, a), CdclkTransition::Unchanged);
    }

    #[test]
    fn crawl_and_squash_uses_the_midpoint_source_formula_and_rejects_unknown_pll() {
        let caps = CdclkTransitionCaps {
            display_version: 12,
            has_crawl: true,
            has_squash: true,
        };
        let old = CdclkConfig {
            cdclk_khz: 408000,
            vco_khz: 1224000,
            refclk_khz: 38400,
            waveform: 0xb6b6,
        };
        let new = CdclkConfig {
            cdclk_khz: 530400,
            vco_khz: 1591200,
            refclk_khz: 38400,
            waveform: 0xf7de,
        };
        let midpoint = crawl_and_squash_midpoint(caps, 800000, old, new).unwrap();
        assert_eq!(midpoint.vco_khz, old.vco_khz);
        assert_eq!(midpoint.waveform, new.waveform);
        assert_eq!(midpoint.cdclk_khz, 497250);
        let unknown = CdclkConfig {
            vco_khz: u32::MAX,
            ..old
        };
        assert_eq!(
            transition(caps, 800000, unknown, new),
            CdclkTransition::FullPll
        );
    }
}
