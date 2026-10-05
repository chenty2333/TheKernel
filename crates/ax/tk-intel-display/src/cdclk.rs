// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_cdclk.c:
// adlp_cdclk_table, _intel_pixel_rate_to_cdclk, bxt_calc_cdclk (table selection).
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
}
