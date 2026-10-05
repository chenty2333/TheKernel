// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_dpll_mgr.c:
// icl_mg_pll_find_divisors, icl_calc_mg_pll_state, icl_ddi_mg_pll_get_freq.
// Copyright © 2006-2016 Intel Corporation.
// intel_{mg,dkl}_phy_regs.h: selected DKL/clock register fields.
// Copyright © 2022 Intel Corporation. MIT permission text: ../LICENSE-MIT.
// ADL-P/N display-13 DKL HDMI, no SSC only. MG PHY, DP/TBT, combo PLL,
// other platforms and hardware enable/disable/WA writes omitted.
// dkl_pll_get_hw_state is translated below with a preserved-selector wrapper.
use crate::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DklPllState {
    pub dco_khz: u32,
    pub refclkin_ctl: u32,
    pub coreclkctl1: u32,
    pub hsclkctl: u32,
    pub div0: u32,
    pub div1: u32,
    pub ssc: u32,
    pub bias: u32,
    pub tdc_coldst_bias: u32,
}
/// HDMI port_clock is TMDS, not always the mode pixel clock: deep color/YUV420
/// must be handled by HDMI compute_config before passing it here. No writes.
pub fn icl_calc_mg_pll_state(
    clock_khz: u32,
    refclk_khz: u32,
    afc_startup: Option<u8>,
) -> Result<DklPllState, Error> {
    // These bounds avoid C's signed intermediate overflow on untrusted input.
    if !(1..=600000).contains(&clock_khz) || afc_startup.is_some_and(|v| v > 7) {
        return Err(Error::Refused);
    }
    let (iref_ndiv, iref_trim) = match refclk_khz {
        19200 => (1, 28),
        24000 => (1, 25),
        38400 => (2, 28),
        _ => return Err(Error::Refused),
    };
    let (dco_khz, hsclkctl) = icl_mg_pll_find_divisors(clock_khz)?;
    let m1div = 2;
    let m2div_int = dco_khz / (refclk_khz * m1div);
    if m2div_int > 255 {
        return Err(Error::Refused);
    }
    let m2div_rem = dco_khz % (refclk_khz * m1div);
    let m2div_frac = (u64::from(m2div_rem) * (1 << 22) / u64::from(refclk_khz * m1div)) as u32;
    let tdc_targetcnt = (2 * 1000 * 100000 * 10 / (132 * refclk_khz) + 5) / 10;
    let feedfwgain = if m2div_rem > 0 {
        m1div * 1000000 * 100 / (dco_khz * 3 / 10)
    } else {
        0
    };
    let (prop_coeff, int_coeff) = if dco_khz >= 9000000 { (5, 10) } else { (4, 8) };
    let mut div0 = (int_coeff << 16) | (prop_coeff << 12) | (m1div << 8) | m2div_int;
    if let Some(afc) = afc_startup {
        div0 |= u32::from(afc) << 25;
    }
    Ok(DklPllState {
        dco_khz,
        refclkin_ctl: 1 << 8,
        coreclkctl1: 5 << 8,
        hsclkctl,
        div0,
        div1: (iref_trim << 16) | tdc_targetcnt,
        ssc: (iref_ndiv << 29) | (4 << 11),
        bias: (if m2div_frac != 0 { 1 << 30 } else { 0 }) | (m2div_frac << 8),
        tdc_coldst_bias: feedfwgain,
    })
}
fn icl_mg_pll_find_divisors(clock_khz: u32) -> Result<(u32, u32), Error> {
    // Preserve upstream search priority, not the numerically closest DCO.
    for (div1, hsdiv) in [(7, 3), (5, 2), (3, 1), (2, 0)] {
        for div2 in (1..=10).rev() {
            let dco = div1 * div2 * clock_khz * 5;
            if !(7992000..=10000000).contains(&dco) {
                continue;
            }
            let tlinedrv = u32::from(div2 >= 2);
            let hsclkctl = (tlinedrv << 14) | (1 << 16) | (hsdiv << 12) | (div2 << 8);
            return Ok((dco, hsclkctl));
        }
    }
    Err(Error::Refused)
}
pub fn icl_ddi_mg_pll_get_freq(state: &DklPllState, refclk_khz: u32) -> Result<u32, Error> {
    if ![19200, 24000, 38400].contains(&refclk_khz) {
        return Err(Error::Refused);
    }
    let m1 = (state.div0 >> 8) & 0xf;
    let m2_int = state.div0 & 0xff;
    if m1 == 0 || m2_int == 0 {
        return Err(Error::Refused);
    }
    let m2_frac = if state.bias & (1 << 30) != 0 {
        (state.bias >> 8) & 0x3fffff
    } else {
        0
    };
    let div1 = match (state.hsclkctl >> 12) & 3 {
        0 => 2,
        1 => 3,
        2 => 5,
        _ => 7,
    };
    let div2 = ((state.hsclkctl >> 8) & 0xf).max(1);
    let value = u64::from(m1) * u64::from(m2_int) * u64::from(refclk_khz)
        + ((u64::from(m1) * u64::from(m2_frac) * u64::from(refclk_khz)) >> 22);
    u32::try_from(value / (5 * div1 * u64::from(div2))).map_err(|_| Error::Refused)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hdmi_1080p60_tc_pll_is_not_the_combo_wrpll() {
        let s = icl_calc_mg_pll_state(148500, 19200, None).unwrap();
        assert_eq!(
            (s.dco_khz, s.refclkin_ctl, s.coreclkctl1, s.hsclkctl),
            (8910000, 0x100, 0x500, 0x15400)
        );
        assert_eq!(
            (s.div0, s.div1, s.ssc, s.bias, s.tdc_coldst_bias),
            (0x842e8, 0x1c004f, 0x20002000, 0x42000000, 0x4a)
        );
        for refclk in [19200, 24000, 38400] {
            let s = icl_calc_mg_pll_state(148500, refclk, None).unwrap();
            assert_eq!(icl_ddi_mg_pll_get_freq(&s, refclk), Ok(148500));
        }
    }
    #[test]
    fn invalid_inputs_fail_before_divide_and_afc_does_not_change_freq() {
        for clock in [0, 1, 10000, 600001, u32::MAX] {
            assert!(icl_calc_mg_pll_state(clock, 19200, None).is_err());
        }
        for refclk in [0, 1, 100000, u32::MAX] {
            assert!(icl_calc_mg_pll_state(148500, refclk, None).is_err());
        }
        assert!(icl_calc_mg_pll_state(148500, 19200, Some(8)).is_err());
        let s = icl_calc_mg_pll_state(148500, 19200, Some(7)).unwrap();
        assert_eq!(s.div0 >> 25, 7);
        assert_eq!(icl_ddi_mg_pll_get_freq(&s, 19200), Ok(148500));
    }
    #[test]
    fn fractional_tmds_readback_follows_upstream_integer_truncation() {
        let s = icl_calc_mg_pll_state(148352, 24000, None).unwrap();
        assert_eq!(icl_ddi_mg_pll_get_freq(&s, 24000), Ok(148351));
    }
}

/// Power reference acquired only if DISPLAY_CORE is already enabled. The
/// backend releases it after the operation (including errors), never wakes an
/// otherwise dark display during firmware discovery.
pub trait PllReadoutIo: crate::dkl_phy::DklIo {
    fn with_display_core_if_enabled<T>(
        &self,
        operation: impl FnOnce() -> Result<T, Error>,
    ) -> Result<Option<T>, Error>;
}

/// Display-13 DKL get_hw_state. `None` means dark power domain / disabled PLL,
/// not a zero-filled state. Raw enable/lock/power evidence is retained separately
/// from the masked configuration used by i915's PLL state comparisons.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DklPllReadout {
    pub enable: u32,
    pub state: DklPllState,
}
pub fn dkl_pll_get_hw_state(
    io: &impl PllReadoutIo,
    port: crate::dkl_phy::TcPort,
    refclk_khz: u32,
    override_afc_startup: bool,
) -> Result<Option<DklPllReadout>, Error> {
    use crate::dkl_phy::with_preserved_selector;
    io.with_display_core_if_enabled(|| {
        let enable = io.read32(port.pll_enable())?;
        if enable & (1 << 31) == 0 {
            return Ok(None);
        }
        with_preserved_selector(io, port, |phy| {
            let refclkin_ctl = phy.read(0x212c)? & (7 << 8);
            let hsclkctl = phy.read(0x20d4)? & ((1 << 16) | (3 << 14) | (3 << 12) | (15 << 8));
            let coreclkctl1 = phy.read(0x20d8)? & (255 << 8);
            let div0 =
                phy.read(0x2200)? & (0x1fffff | if override_afc_startup { 7 << 25 } else { 0 });
            let div1 = phy.read(0x2204)? & ((31 << 16) | 255);
            let ssc = phy.read(0x2210)? & ((7 << 29) | (255 << 16) | (7 << 11) | (1 << 9));
            let bias = phy.read(0x2214)? & ((1 << 30) | (0x3fffff << 8));
            let tdc_coldst_bias = phy.read(0x2218)? & 0xffff;
            let mut state = DklPllState {
                dco_khz: 0,
                refclkin_ctl,
                coreclkctl1,
                hsclkctl,
                div0,
                div1,
                ssc,
                bias,
                tdc_coldst_bias,
            };
            // dco_khz is derived metadata, not an MMIO state field in i915.
            icl_ddi_mg_pll_get_freq(&state, refclk_khz)?;
            let m1 = (div0 >> 8) & 15;
            let frac = if bias & (1 << 30) != 0 {
                (bias >> 8) & 0x3fffff
            } else {
                0
            };
            let dco = u64::from(m1) * u64::from(div0 & 255) * u64::from(refclk_khz)
                + ((u64::from(m1) * u64::from(frac) * u64::from(refclk_khz)) >> 22);
            state.dco_khz = dco.try_into().map_err(|_| Error::Refused)?;
            Ok(Some(DklPllReadout { enable, state }))
        })
    })
    .map(Option::flatten)
}
