// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_dpll_mgr.c:
// icl_mg_pll_find_divisors, icl_calc_mg_pll_state, icl_ddi_mg_pll_get_freq,
// icl_wrpll_get_multipliers, icl_wrpll_params_populate, icl_calc_wrpll,
// icl_calc_dp_combo_pll, icl_calc_tbt_pll, icl_calc_dpll_state,
// icl_ddi_combo_pll_get_freq, icl_tc_port_to_pll_id, icl_update_active_dpll,
// icl_get_combo_phy_dpll, icl_plls/tgl_plls/rkl_plls/adls_plls/adlp_plls,
// intel_find_dpll/reference/unreference,
// dkl_pll_write (ADL-P/N DKL no-SSC branch).
// Copyright © 2006-2016 Intel Corporation.
// intel_{mg,dkl}_phy_regs.h: selected DKL/clock register fields.
// Copyright © 2022 Intel Corporation. MIT permission text: ../LICENSE-MIT.
// ADL-P/N display-13 DKL and ICL/TGL combo PLL arithmetic/CFGCR fields.
// MG PHY hardware sequencing, PLL allocation/refcounts, and full DPLL manager
// init/readout/enable/disable integration remain outside this file. DKL readout
// restores the shared selector; programming preserves i915 RMW order.
use crate::{
    Error,
    dkl_phy::{DklIo, DklRegister, TcPort, intel_dkl_phy_posting_read, intel_dkl_phy_rmw},
};

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MgPllOutput {
    DisplayPort,
    Hdmi,
}

/// Calculate the ADL-P/N DKL Type-C PLL state for DP symbol rate or HDMI TMDS
/// character rate. HDMI `clock_khz` is not always the pixel clock: color depth
/// and YUV420 are handled by HDMI compute_config before reaching this helper.
// upstream: intel_dpll_mgr.c icl_calc_mg_pll_state()
pub fn icl_calc_mg_pll_state_for_output(
    clock_khz: u32,
    refclk_khz: u32,
    output: MgPllOutput,
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
    let (dco_khz, hsclkctl, coreclkctl1) =
        icl_mg_pll_find_divisors(clock_khz, output, false, true)?;
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
        coreclkctl1,
        hsclkctl,
        div0,
        div1: (iref_trim << 16) | tdc_targetcnt,
        ssc: (iref_ndiv << 29) | (4 << 11),
        bias: (if m2div_frac != 0 { 1 << 30 } else { 0 }) | (m2div_frac << 8),
        tdc_coldst_bias: feedfwgain,
    })
}

/// Preserve the source's HS divisor and DS divisor loop ordering, and choose
/// the exact DCO target for DP versus the wider HDMI window.
// upstream: intel_dpll_mgr.c icl_mg_pll_find_divisors()
fn icl_mg_pll_find_divisors(
    clock_khz: u32,
    output: MgPllOutput,
    use_ssc: bool,
    is_dkl: bool,
) -> Result<(u32, u32, u32), Error> {
    let (dco_min, dco_max) = match output {
        MgPllOutput::DisplayPort => (8_100_000, 8_100_000),
        MgPllOutput::Hdmi if use_ssc => (8_000_000, 10_000_000),
        MgPllOutput::Hdmi => (7_992_000, 10_000_000),
    };
    for (div1, hsdiv) in [(7, 3), (5, 2), (3, 1), (2, 0)] {
        for div2 in (1..=10).rev() {
            let dco = div1 * div2 * clock_khz * 5;
            if !(dco_min..=dco_max).contains(&dco) {
                continue;
            }
            let is_dp = output == MgPllOutput::DisplayPort;
            let a_divratio = if div2 >= 2 {
                if is_dp { 10 } else { 5 }
            } else {
                5
            };
            let tlinedrv = if div2 >= 2 {
                if is_dkl { 1 } else { 2 }
            } else {
                0
            };
            let inputsel = u32::from(!is_dp);
            let hsclkctl = (tlinedrv << 14) | (inputsel << 16) | (hsdiv << 12) | (div2 << 8);
            return Ok((dco, hsclkctl, a_divratio << 8));
        }
    }
    Err(Error::Refused)
}

/// HDMI convenience wrapper for existing call sites.
pub fn icl_calc_mg_pll_state(
    clock_khz: u32,
    refclk_khz: u32,
    afc_startup: Option<u8>,
) -> Result<DklPllState, Error> {
    icl_calc_mg_pll_state_for_output(clock_khz, refclk_khz, MgPllOutput::Hdmi, afc_startup)
}
// upstream: intel_dpll_mgr.c icl_ddi_mg_pll_get_freq()
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

/// Display-12/13 `dkl_pll_write`, called only after the selected DKL PLL is
/// disabled and its TC port/core power references are held. The field masks
/// and unconditional RMW stores match i915; the caller owns enable/lock
/// polling and the before-image transaction.
// upstream: intel_dpll_mgr.c dkl_pll_write()
pub fn dkl_pll_write(
    io: &impl DklIo,
    port: TcPort,
    state: &DklPllState,
    afc_startup: Option<u8>,
) -> Result<(), Error> {
    if afc_startup.is_some_and(|value| value > 7) {
        return Err(Error::Refused);
    }
    let write =
        |offset, clear, value| intel_dkl_phy_rmw(io, DklRegister::new(port, offset)?, clear, value);
    write(0x212c, 7 << 8, state.refclkin_ctl)?;
    write(0x20d8, 0xff << 8, state.coreclkctl1)?;
    write(
        0x20d4,
        (3 << 14) | (1 << 16) | (3 << 12) | (15 << 8),
        state.hsclkctl,
    )?;
    write(
        0x2200,
        0x1f_0000
            | (0xf << 12)
            | (0xf << 8)
            | 0xff
            | if afc_startup.is_some() { 7 << 25 } else { 0 },
        state.div0 | afc_startup.map_or(0, |value| u32::from(value) << 25),
    )?;
    write(0x2204, (0x1f << 16) | 0xff, state.div1)?;
    write(
        0x2210,
        (7 << 29) | (0xff << 16) | (7 << 11) | (1 << 9),
        state.ssc,
    )?;
    write(0x2214, (1 << 30) | (0x3f_ffff << 8), state.bias)?;
    write(0x2218, (0xff << 8) | 0xff, state.tdc_coldst_bias)?;
    intel_dkl_phy_posting_read(io, DklRegister::new(port, 0x2218)?)
}
#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::{collections::BTreeMap, sync::Mutex as StdMutex, vec::Vec};

    #[derive(Default)]
    struct ModelState {
        selector: u32,
        dkl: BTreeMap<(u32, u32), u32>,
        operations: Vec<(bool, u32, u32)>,
        fail_at: Option<usize>,
    }
    #[derive(Default)]
    struct Model {
        state: StdMutex<ModelState>,
        dkl_lock: StdMutex<()>,
    }
    impl crate::RegisterIo for Model {
        fn read32(&self, offset: u32) -> Result<u32, Error> {
            let mut state = self.state.lock().unwrap();
            let value = if offset == 0x1010a0 {
                state.selector
            } else if (0x168000..0x16c000).contains(&offset) {
                let port = (offset - 0x168000) / 0x1000;
                let internal = ((state.selector >> (port * 8)) & 15) * 0x1000 + (offset & 0xfff);
                *state.dkl.get(&(port, internal)).unwrap_or(&0)
            } else {
                return Err(Error::Unavailable(offset));
            };
            state.operations.push((false, offset, value));
            if state.fail_at == Some(state.operations.len()) {
                return Err(Error::Unavailable(offset));
            }
            Ok(value)
        }
        fn write32(&self, offset: u32, value: u32) -> Result<(), Error> {
            let mut state = self.state.lock().unwrap();
            state.operations.push((true, offset, value));
            if offset == 0x1010a0 {
                state.selector = value;
            } else if (0x168000..0x16c000).contains(&offset) {
                let port = (offset - 0x168000) / 0x1000;
                let internal = ((state.selector >> (port * 8)) & 15) * 0x1000 + (offset & 0xfff);
                state.dkl.insert((port, internal), value);
            } else {
                return Err(Error::Unavailable(offset));
            }
            if state.fail_at == Some(state.operations.len()) {
                // Model a failed accessor whose store landed in hardware.
                return Err(Error::Unavailable(offset));
            }
            Ok(())
        }
    }
    impl DklIo for Model {
        fn with_dkl_lock<T>(
            &self,
            operation: impl FnOnce() -> Result<T, Error>,
        ) -> Result<T, Error> {
            let _lock = self.dkl_lock.lock().unwrap();
            operation()
        }
    }

    fn initial_dkl(port: TcPort) -> Model {
        let model = Model::default();
        let mut state = model.state.lock().unwrap();
        for offset in [
            0x212c, 0x20d8, 0x20d4, 0x2200, 0x2204, 0x2210, 0x2214, 0x2218,
        ] {
            state
                .dkl
                .insert((port.index(), offset), 0xa5a5_5a5a ^ offset);
        }
        drop(state);
        model
    }

    #[test]
    fn dkl_write_uses_i915_rmw_order_masks_and_tc2_window() {
        let port = TcPort::Tc2;
        let model = initial_dkl(port);
        let before = model.state.lock().unwrap().dkl.clone();
        let pll = icl_calc_mg_pll_state(148500, 19200, Some(7)).unwrap();
        dkl_pll_write(&model, port, &pll, Some(7)).unwrap();
        let state = model.state.lock().unwrap();
        let writes: Vec<_> = state
            .operations
            .iter()
            .filter(|(write, offset, _)| *write && *offset != 0x1010a0)
            .map(|(_, offset, _)| *offset)
            .collect();
        assert_eq!(
            writes,
            [
                0x212c, 0x20d8, 0x20d4, 0x2200, 0x2204, 0x2210, 0x2214, 0x2218
            ]
            .map(|offset| 0x169000 + (offset & 0xfff))
        );
        let expected = [
            (0x212c, 7 << 8, pll.refclkin_ctl),
            (0x20d8, 0xff << 8, pll.coreclkctl1),
            (
                0x20d4,
                (3 << 14) | (1 << 16) | (3 << 12) | (15 << 8),
                pll.hsclkctl,
            ),
            (
                0x2200,
                0x1f_0000 | (0xf << 12) | (0xf << 8) | 0xff | (7 << 25),
                pll.div0,
            ),
            (0x2204, (0x1f << 16) | 0xff, pll.div1),
            (
                0x2210,
                (7 << 29) | (0xff << 16) | (7 << 11) | (1 << 9),
                pll.ssc,
            ),
            (0x2214, (1 << 30) | (0x3f_ffff << 8), pll.bias),
            (0x2218, (0xff << 8) | 0xff, pll.tdc_coldst_bias),
        ];
        for (offset, mask, value) in expected {
            let old = before[&(port.index(), offset)];
            assert_eq!(
                state.dkl[&(port.index(), offset)],
                (old & !mask) | (value & mask)
            );
        }
    }

    #[test]
    fn dkl_program_stops_at_every_failed_mmio_prefix() {
        let state = icl_calc_mg_pll_state(148500, 19200, None).unwrap();
        for failure in 1..=26 {
            let model = initial_dkl(TcPort::Tc1);
            model.state.lock().unwrap().fail_at = Some(failure);
            assert!(dkl_pll_write(&model, TcPort::Tc1, &state, None).is_err());
            assert_eq!(model.state.lock().unwrap().operations.len(), failure);
        }
    }

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
    fn dkl_displayport_uses_exact_8100_mhz_dco_and_dp_source_fields() {
        let state =
            icl_calc_mg_pll_state_for_output(540_000, 19_200, MgPllOutput::DisplayPort, None)
                .unwrap();
        assert_eq!(state.dco_khz, 8_100_000);
        assert_eq!(state.coreclkctl1, 5 << 8);
        assert_eq!(state.hsclkctl, (1 << 12) | (1 << 8));
        assert_eq!(icl_ddi_mg_pll_get_freq(&state, 19_200), Ok(540_000));

        let state =
            icl_calc_mg_pll_state_for_output(162_000, 38_400, MgPllOutput::DisplayPort, None)
                .unwrap();
        assert_eq!(state.dco_khz, 8_100_000);
        assert_eq!(state.coreclkctl1, 10 << 8);
        assert_eq!(state.hsclkctl, (1 << 14) | (2 << 12) | (2 << 8));
        assert_eq!(icl_ddi_mg_pll_get_freq(&state, 38_400), Ok(162_000));
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

/// The subset of `skl_wrpll_params` used by display-12/13 combo PHY PLLs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IclWrpllParams {
    pub dco_integer: u32,
    pub dco_fraction: u32,
    /// Hardware code: P={2,3,5,7} maps to {1,2,4,8}.
    pub pdiv: u32,
    /// Hardware code: K={1,2,3} maps to {1,2,4}.
    pub kdiv: u32,
    pub qdiv_mode: u32,
    pub qdiv_ratio: u32,
}

/// The ICL/TGL DPLL CFGCR image produced by `icl_calc_dpll_state()`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IclComboPllState {
    pub cfgcr0: u32,
    pub cfgcr1: u32,
    /// AFC startup override lives in DPLL0_DIV0, not in CFGCR0/1.
    pub div0: u32,
}

const DCO_MIN_KHZ: u32 = 7_998_000;
const DCO_MAX_KHZ: u32 = 10_000_000;
const DCO_MID_KHZ: u32 = (DCO_MIN_KHZ + DCO_MAX_KHZ) / 2;
const ICL_WRPLL_DIVIDERS: [u32; 46] = [
    2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40, 42, 44, 48, 50, 52, 54, 56, 60, 64,
    66, 68, 70, 72, 76, 78, 80, 84, 88, 90, 92, 96, 98, 100, 102, 3, 5, 7, 9, 15, 21,
];

/// Select the integer `(P, Q, K)` factors for one source-listed total divider.
// upstream: intel_dpll_mgr.c icl_wrpll_get_multipliers()
pub fn icl_wrpll_get_multipliers(best_div: u32) -> Result<(u32, u32, u32), Error> {
    if best_div == 2 {
        return Ok((2, 1, 1));
    }
    if best_div % 2 == 0 {
        if best_div % 4 == 0 {
            return Ok((2, best_div / 4, 2));
        }
        if best_div % 6 == 0 {
            return Ok((3, best_div / 6, 2));
        }
        if best_div % 5 == 0 {
            return Ok((5, best_div / 10, 2));
        }
        if best_div % 14 == 0 {
            return Ok((7, best_div / 14, 2));
        }
        return Err(Error::Refused);
    }
    if [3, 5, 7].contains(&best_div) {
        Ok((best_div, 1, 1))
    } else if [9, 15, 21].contains(&best_div) {
        Ok((best_div / 3, 1, 3))
    } else {
        Err(Error::Refused)
    }
}

/// Encode the logical ICL WRPLL divisors and fixed-point DCO into hardware fields.
// upstream: intel_dpll_mgr.c icl_wrpll_params_populate()
pub fn icl_wrpll_params_populate(
    dco_freq_khz: u32,
    ref_freq_khz: u32,
    pdiv: u32,
    qdiv: u32,
    kdiv: u32,
) -> Result<IclWrpllParams, Error> {
    if ref_freq_khz == 0 || (kdiv != 2 && qdiv != 1) {
        return Err(Error::Refused);
    }
    let pdiv_code = match pdiv {
        2 => 1,
        3 => 2,
        5 => 4,
        7 => 8,
        _ => return Err(Error::Refused),
    };
    let kdiv_code = match kdiv {
        1 => 1,
        2 => 2,
        3 => 4,
        _ => return Err(Error::Refused),
    };
    if qdiv == 0 || qdiv > 0xff {
        return Err(Error::Refused);
    }
    let dco = (u64::from(dco_freq_khz) << 15) / u64::from(ref_freq_khz);
    Ok(IclWrpllParams {
        dco_integer: (dco >> 15) as u32,
        dco_fraction: (dco & 0x7fff) as u32,
        pdiv: pdiv_code,
        kdiv: kdiv_code,
        qdiv_mode: u32::from(qdiv != 1),
        qdiv_ratio: qdiv,
    })
}

/// Use half of the 38.4-MHz reference on ICL+; hardware divides it by two.
// upstream: intel_dpll_mgr.c icl_wrpll_ref_clock()
pub const fn icl_wrpll_ref_clock(ref_clock_khz: u32) -> Result<u32, Error> {
    match ref_clock_khz {
        19_200 | 24_000 => Ok(ref_clock_khz),
        38_400 => Ok(19_200),
        _ => Err(Error::Refused),
    }
}

/// Search the source's flat candidate list for the DCO closest to 8999 MHz.
// upstream: intel_dpll_mgr.c icl_calc_wrpll()
pub fn icl_calc_wrpll(port_clock_khz: u32, ref_clock_khz: u32) -> Result<IclWrpllParams, Error> {
    if port_clock_khz == 0 || port_clock_khz > 1_000_000 {
        return Err(Error::Refused);
    }
    let ref_clock_khz = icl_wrpll_ref_clock(ref_clock_khz)?;
    let afe_clock_khz = port_clock_khz.checked_mul(5).ok_or(Error::Refused)?;
    let mut best: Option<(u32, u32)> = None;
    for divider in ICL_WRPLL_DIVIDERS {
        let dco = afe_clock_khz.checked_mul(divider).ok_or(Error::Refused)?;
        if (DCO_MIN_KHZ..=DCO_MAX_KHZ).contains(&dco) {
            let centrality = dco.abs_diff(DCO_MID_KHZ);
            if best.is_none_or(|(_, current)| centrality < current) {
                best = Some((divider, centrality));
            }
        }
    }
    let (best_div, _) = best.ok_or(Error::Refused)?;
    let (pdiv, qdiv, kdiv) = icl_wrpll_get_multipliers(best_div)?;
    icl_wrpll_params_populate(afe_clock_khz * best_div, ref_clock_khz, pdiv, qdiv, kdiv)
}

const DP_COMBO_24: [(u32, u32, u32, u32, u32, u32); 8] = [
    (540000, 0x151, 0x4000, 2, 1, 0),
    (270000, 0x151, 0x4000, 2, 2, 0),
    (162000, 0x151, 0x4000, 4, 2, 0),
    (324000, 0x151, 0x4000, 4, 1, 0),
    (216000, 0x168, 0, 1, 2, 2),
    (432000, 0x168, 0, 1, 2, 0),
    (648000, 0x195, 0, 2, 1, 0),
    (810000, 0x151, 0x4000, 1, 1, 0),
];
const DP_COMBO_19_2: [(u32, u32, u32, u32, u32, u32); 8] = [
    (540000, 0x1a5, 0x7000, 2, 1, 0),
    (270000, 0x1a5, 0x7000, 2, 2, 0),
    (162000, 0x1a5, 0x7000, 4, 2, 0),
    (324000, 0x1a5, 0x7000, 4, 1, 0),
    (216000, 0x1c2, 0, 1, 2, 2),
    (432000, 0x1c2, 0, 1, 2, 0),
    (648000, 0x1fa, 0x2000, 2, 1, 0),
    (810000, 0x1a5, 0x7000, 1, 1, 0),
];

fn table_wrpll(row: (u32, u32, u32, u32, u32, u32)) -> IclWrpllParams {
    IclWrpllParams {
        dco_integer: row.1,
        dco_fraction: row.2,
        pdiv: row.3,
        kdiv: row.4,
        qdiv_mode: u32::from(row.5 != 0),
        qdiv_ratio: row.5,
    }
}

/// Fixed DisplayPort combo-PLL table for 24 MHz and 19.2/38.4 MHz references.
// upstream: intel_dpll_mgr.c icl_calc_dp_combo_pll()
pub fn icl_calc_dp_combo_pll(
    port_clock_khz: u32,
    ref_clock_khz: u32,
) -> Result<IclWrpllParams, Error> {
    let table = match ref_clock_khz {
        24_000 => &DP_COMBO_24,
        19_200 | 38_400 => &DP_COMBO_19_2,
        _ => return Err(Error::Refused),
    };
    table
        .iter()
        .find(|row| row.0 == port_clock_khz)
        .copied()
        .map(table_wrpll)
        .ok_or(Error::Refused)
}

/// Fixed TBT PLL parameters. Display 12 uses the TGL table; earlier ICL used
/// the ICL table. At the 38.4-MHz reference, source selects the 19.2-MHz row.
// upstream: intel_dpll_mgr.c icl_calc_tbt_pll()
pub fn icl_calc_tbt_pll(display_version: u8, ref_clock_khz: u32) -> Result<IclWrpllParams, Error> {
    let ref_clock_khz = match ref_clock_khz {
        19_200 | 38_400 => 19_200,
        24_000 => 24_000,
        _ => return Err(Error::Refused),
    };
    let (dco_integer, dco_fraction, pdiv, kdiv) = if display_version >= 12 {
        if ref_clock_khz == 19_200 {
            (0x54, 0x3000, 0, 0)
        } else {
            (0x43, 0x4000, 0, 0)
        }
    } else if ref_clock_khz == 19_200 {
        (0x1a5, 0x7000, 4, 1)
    } else {
        (0x151, 0x4000, 4, 1)
    };
    Ok(IclWrpllParams {
        dco_integer,
        dco_fraction,
        pdiv,
        kdiv,
        qdiv_mode: 0,
        qdiv_ratio: 0,
    })
}

/// Encode WRPLL state into CFGCR0/1, including the source's 38.4-MHz fraction
/// workaround and optional TGL AFC startup override.
// upstream: intel_dpll_mgr.c icl_calc_dpll_state()
pub fn icl_calc_dpll_state(
    params: IclWrpllParams,
    display_version: u8,
    reference_khz: u32,
    override_afc_startup: Option<u8>,
) -> Result<IclComboPllState, Error> {
    if override_afc_startup.is_some_and(|v| v > 7) || params.dco_fraction > 0x7fff {
        return Err(Error::Refused);
    }
    let dco_fraction = if display_version >= 12 && reference_khz == 38_400 {
        (params.dco_fraction + 1) / 2
    } else {
        params.dco_fraction
    };
    let cfgcr0 = (dco_fraction << 10) | (params.dco_integer & 0x3ff);
    let cfgcr1 = (params.qdiv_ratio << 10)
        | (params.qdiv_mode << 9)
        | (params.kdiv << 6)
        | (params.pdiv << 2)
        | if display_version >= 12 { 0 } else { 3 };
    Ok(IclComboPllState {
        cfgcr0,
        cfgcr1,
        div0: override_afc_startup.map_or(0, |value| u32::from(value) << 25),
    })
}

/// Decode combo-PLL CFGCR0/1 back to the source's TMDS/symbol frequency.
// upstream: intel_dpll_mgr.c icl_ddi_combo_pll_get_freq()
pub fn icl_ddi_combo_pll_get_freq(
    state: IclComboPllState,
    ref_clock_khz: u32,
    display_version: u8,
    frac_div_wa: bool,
) -> Result<u32, Error> {
    let raw_ref_clock_khz = ref_clock_khz;
    let ref_clock_khz = icl_wrpll_ref_clock(raw_ref_clock_khz)?;
    let pdiv = match (state.cfgcr1 >> 2) & 0xf {
        1 => 2,
        2 => 3,
        4 => 5,
        8 => 7,
        _ => return Err(Error::Refused),
    };
    let kdiv = match (state.cfgcr1 >> 6) & 7 {
        1 => 1,
        2 => 2,
        4 => 3,
        _ => return Err(Error::Refused),
    };
    let qdiv = if state.cfgcr1 & (1 << 9) != 0 {
        (state.cfgcr1 >> 10) & 0xff
    } else {
        1
    };
    let mut fraction = (state.cfgcr0 >> 10) & 0x7fff;
    if frac_div_wa && display_version >= 12 && raw_ref_clock_khz == 38_400 {
        fraction *= 2;
    }
    let dco = (state.cfgcr0 & 0x3ff) * ref_clock_khz + (fraction * ref_clock_khz) / 0x8000;
    let divider = pdiv * qdiv * kdiv * 5;
    (divider != 0)
        .then_some(dco / divider)
        .ok_or(Error::Refused)
}

/// Shared DPLL identities used by the ICL/TGL manager (`intel_dpll_mgr.h`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum IclDpllId {
    Dpll0 = 0,
    Dpll1 = 1,
    Tbt   = 2,
    Mg1   = 3,
    Mg2   = 4,
    Mg3   = 5,
    Mg4   = 6,
    Mg5   = 7,
    Mg6   = 8,
}

/// The hardware image compared by the source's shared DPLL allocator.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IclDpllHwState {
    pub cfgcr0: u32,
    pub cfgcr1: u32,
    pub div0: u32,
    pub mg: [u32; 12],
}

/// Map TC1..TC6 to the corresponding dedicated MG PLL.
// upstream: intel_dpll_mgr.c icl_tc_port_to_pll_id()
pub const fn icl_tc_port_to_pll_id(tc_port: u8) -> Result<IclDpllId, Error> {
    match tc_port {
        0 => Ok(IclDpllId::Mg1),
        1 => Ok(IclDpllId::Mg2),
        2 => Ok(IclDpllId::Mg3),
        3 => Ok(IclDpllId::Mg4),
        4 => Ok(IclDpllId::Mg5),
        5 => Ok(IclDpllId::Mg6),
        _ => Err(Error::Refused),
    }
}

/// The active port PLL is MG only for TC legacy/DP-alt modes; combo ports use
/// the default DPLL, matching `icl_update_active_dpll()`.
// upstream: intel_dpll_mgr.c icl_update_active_dpll()
pub const fn icl_active_port_dpll_id(is_tc: bool, in_alt_or_legacy_mode: bool) -> u8 {
    if is_tc && in_alt_or_legacy_mode { 1 } else { 0 }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IclDpllPlatform {
    AlderLakeS,
    Dg1,
    RocketLake,
    ElkhartLake,
    JasperLake,
    TigerLake,
    AlderLakeP,
    AlderLakeN,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IclDpllKind {
    Combo,
    Tbt,
    Dkl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IclDpllDescriptor {
    pub name: &'static str,
    pub id: u8,
    pub kind: IclDpllKind,
    pub is_alt_port_dpll: bool,
}

const DPLL0: IclDpllDescriptor = IclDpllDescriptor {
    name: "DPLL 0",
    id: 0,
    kind: IclDpllKind::Combo,
    is_alt_port_dpll: false,
};
const DPLL1: IclDpllDescriptor = IclDpllDescriptor {
    name: "DPLL 1",
    id: 1,
    kind: IclDpllKind::Combo,
    is_alt_port_dpll: false,
};
const TBT_PLL: IclDpllDescriptor = IclDpllDescriptor {
    name: "TBT PLL",
    id: 2,
    kind: IclDpllKind::Tbt,
    is_alt_port_dpll: true,
};
const DPLL2: IclDpllDescriptor = IclDpllDescriptor {
    name: "DPLL 2",
    id: 2,
    kind: IclDpllKind::Combo,
    is_alt_port_dpll: false,
};
const DPLL3: IclDpllDescriptor = IclDpllDescriptor {
    name: "DPLL 3",
    id: 3,
    kind: IclDpllKind::Combo,
    is_alt_port_dpll: false,
};
const DPLL4: IclDpllDescriptor = IclDpllDescriptor {
    name: "DPLL 4",
    id: 2,
    kind: IclDpllKind::Combo,
    is_alt_port_dpll: false,
};
const TC_PLL1: IclDpllDescriptor = IclDpllDescriptor {
    name: "TC PLL 1",
    id: 3,
    kind: IclDpllKind::Dkl,
    is_alt_port_dpll: false,
};
const TC_PLL2: IclDpllDescriptor = IclDpllDescriptor {
    name: "TC PLL 2",
    id: 4,
    kind: IclDpllKind::Dkl,
    is_alt_port_dpll: false,
};
const TC_PLL3: IclDpllDescriptor = IclDpllDescriptor {
    name: "TC PLL 3",
    id: 5,
    kind: IclDpllKind::Dkl,
    is_alt_port_dpll: false,
};
const TC_PLL4: IclDpllDescriptor = IclDpllDescriptor {
    name: "TC PLL 4",
    id: 6,
    kind: IclDpllKind::Dkl,
    is_alt_port_dpll: false,
};
const TC_PLL5: IclDpllDescriptor = IclDpllDescriptor {
    name: "TC PLL 5",
    id: 7,
    kind: IclDpllKind::Dkl,
    is_alt_port_dpll: false,
};
const TC_PLL6: IclDpllDescriptor = IclDpllDescriptor {
    name: "TC PLL 6",
    id: 8,
    kind: IclDpllKind::Dkl,
    is_alt_port_dpll: false,
};
const TGL_PLLS: &[IclDpllDescriptor] = &[
    DPLL0, DPLL1, TBT_PLL, TC_PLL1, TC_PLL2, TC_PLL3, TC_PLL4, TC_PLL5, TC_PLL6,
];
const ICL_PLLS: &[IclDpllDescriptor] = &[DPLL0, DPLL1, TBT_PLL, TC_PLL1, TC_PLL2, TC_PLL3, TC_PLL4];
const RKL_PLLS: &[IclDpllDescriptor] = &[DPLL0, DPLL1, DPLL4];
const ADLS_PLLS: &[IclDpllDescriptor] = &[DPLL0, DPLL1, DPLL2, DPLL3];
const DG1_PLLS: &[IclDpllDescriptor] = &[DPLL0, DPLL1, DPLL2, DPLL3];

/// Return the platform's DPLL inventory in i915 declaration order.
// upstream: intel_dpll_mgr.c icl_plls/ehl_plls/tgl_plls/rkl_plls/dg1_plls/adls_plls/adlp_plls
pub const fn icl_dpll_descriptors(platform: IclDpllPlatform) -> &'static [IclDpllDescriptor] {
    match platform {
        IclDpllPlatform::AlderLakeS => ADLS_PLLS,
        IclDpllPlatform::Dg1 => DG1_PLLS,
        IclDpllPlatform::RocketLake => RKL_PLLS,
        IclDpllPlatform::ElkhartLake | IclDpllPlatform::JasperLake => RKL_PLLS,
        IclDpllPlatform::TigerLake => TGL_PLLS,
        IclDpllPlatform::AlderLakeP | IclDpllPlatform::AlderLakeN => ICL_PLLS,
    }
}

/// Candidate bitmap from `icl_get_combo_phy_dpll()` minus HTI-owned PLLs.
// upstream: intel_dpll_mgr.c icl_get_combo_phy_dpll()
pub const fn icl_combo_dpll_mask(platform: IclDpllPlatform, port: u8, hti_mask: u32) -> u32 {
    let available = match platform {
        IclDpllPlatform::AlderLakeS => 0b1111,
        IclDpllPlatform::Dg1 if port == 3 || port == 4 => 0b1100,
        IclDpllPlatform::Dg1 => 0b0011,
        IclDpllPlatform::RocketLake
        | IclDpllPlatform::ElkhartLake
        | IclDpllPlatform::JasperLake
            if port != 0 =>
        {
            0b0111
        }
        _ => 0b0011,
    };
    available & !hti_mask
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SharedDpllSlot {
    id: u8,
    state: Option<IclDpllHwState>,
    pipe_mask: u8,
}
impl SharedDpllSlot {
    const EMPTY: Self = Self {
        id: u8::MAX,
        state: None,
        pipe_mask: 0,
    };
}

/// Source-shaped state owner for `intel_find_dpll()` and the reference/put pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SharedDpllPool {
    slots: [SharedDpllSlot; 9],
}
impl SharedDpllPool {
    pub const fn new() -> Self {
        Self {
            slots: [SharedDpllSlot::EMPTY; 9],
        }
    }

    /// Reuse identical state, otherwise take the first free DPLL in candidate order.
    // upstream: intel_dpll_mgr.c intel_find_dpll()/intel_reference_dpll()
    pub fn find_and_reference(
        &mut self,
        candidate_mask: u32,
        state: IclDpllHwState,
        pipe: u8,
    ) -> Result<IclDpllId, Error> {
        if pipe >= 8 || candidate_mask == 0 {
            return Err(Error::Refused);
        }
        let bit = 1 << pipe;
        if let Some(slot) = self.slots.iter_mut().find(|s| {
            s.id != u8::MAX && candidate_mask & (1 << s.id) != 0 && s.state == Some(state)
        }) {
            slot.pipe_mask |= bit;
            return dpll_id(slot.id);
        }
        let id = (0..9)
            .find(|id| candidate_mask & (1 << id) != 0 && !self.slots.iter().any(|s| s.id == *id))
            .ok_or(Error::Refused)?;
        let slot = self
            .slots
            .iter_mut()
            .find(|s| s.id == u8::MAX)
            .ok_or(Error::Refused)?;
        *slot = SharedDpllSlot {
            id,
            state: Some(state),
            pipe_mask: bit,
        };
        dpll_id(id)
    }

    /// Remove a pipe reference; return true when the last reference powers the PLL down.
    // upstream: intel_dpll_mgr.c intel_unreference_dpll()
    pub fn unreference(&mut self, id: IclDpllId, pipe: u8) -> Result<bool, Error> {
        if pipe >= 8 {
            return Err(Error::Refused);
        }
        let slot = self
            .slots
            .iter_mut()
            .find(|s| s.id == id as u8)
            .ok_or(Error::Refused)?;
        let bit = 1 << pipe;
        if slot.pipe_mask & bit == 0 {
            return Err(Error::Refused);
        }
        slot.pipe_mask &= !bit;
        let last = slot.pipe_mask == 0;
        if last {
            *slot = SharedDpllSlot::EMPTY;
        }
        Ok(last)
    }

    pub fn active_pipe_mask(&self, id: IclDpllId) -> u8 {
        self.slots
            .iter()
            .find(|s| s.id == id as u8)
            .map_or(0, |s| s.pipe_mask)
    }
}

impl Default for SharedDpllPool {
    fn default() -> Self {
        Self::new()
    }
}

const fn dpll_id(value: u8) -> Result<IclDpllId, Error> {
    match value {
        0 => Ok(IclDpllId::Dpll0),
        1 => Ok(IclDpllId::Dpll1),
        2 => Ok(IclDpllId::Tbt),
        3 => Ok(IclDpllId::Mg1),
        4 => Ok(IclDpllId::Mg2),
        5 => Ok(IclDpllId::Mg3),
        6 => Ok(IclDpllId::Mg4),
        7 => Ok(IclDpllId::Mg5),
        8 => Ok(IclDpllId::Mg6),
        _ => Err(Error::Refused),
    }
}

#[cfg(test)]
mod combo_tests {
    use super::*;

    #[test]
    fn shared_manager_reuses_matching_pll_and_releases_after_last_pipe() {
        let state = IclDpllHwState {
            cfgcr0: 0x1234,
            cfgcr1: 0x5678,
            ..IclDpllHwState::default()
        };
        let mut pool = SharedDpllPool::new();
        let first = pool.find_and_reference(0b11, state, 0).unwrap();
        assert_eq!(first, IclDpllId::Dpll0);
        assert_eq!(
            pool.find_and_reference(0b11, state, 1),
            Ok(IclDpllId::Dpll0)
        );
        assert_eq!(pool.active_pipe_mask(first), 0b11);
        assert_eq!(pool.unreference(first, 0), Ok(false));
        assert_eq!(pool.unreference(first, 1), Ok(true));
        assert_eq!(pool.unreference(first, 1), Err(Error::Refused));
    }

    #[test]
    fn source_combo_masks_and_tc_mg_ids_are_platform_specific() {
        assert_eq!(
            icl_combo_dpll_mask(IclDpllPlatform::TigerLake, 1, 0),
            0b0011
        );
        assert_eq!(
            icl_combo_dpll_mask(IclDpllPlatform::RocketLake, 1, 0),
            0b0111
        );
        assert_eq!(icl_combo_dpll_mask(IclDpllPlatform::Dg1, 3, 0), 0b1100);
        assert_eq!(
            icl_combo_dpll_mask(IclDpllPlatform::AlderLakeS, 0, 0b0010),
            0b1101
        );
        assert_eq!(icl_tc_port_to_pll_id(0), Ok(IclDpllId::Mg1));
        assert_eq!(icl_tc_port_to_pll_id(5), Ok(IclDpllId::Mg6));
        assert_eq!(icl_tc_port_to_pll_id(6), Err(Error::Refused));
        assert_eq!(icl_active_port_dpll_id(true, true), 1);
        assert_eq!(icl_active_port_dpll_id(false, true), 0);
    }

    #[test]
    fn per_platform_dpll_inventory_matches_i915_tables() {
        let tgl = icl_dpll_descriptors(IclDpllPlatform::TigerLake);
        assert_eq!(tgl.len(), 9);
        assert_eq!(tgl[2], TBT_PLL);
        assert_eq!(tgl[8], TC_PLL6);

        for platform in [IclDpllPlatform::AlderLakeP, IclDpllPlatform::AlderLakeN] {
            let adlp = icl_dpll_descriptors(platform);
            assert_eq!(adlp.len(), 7);
            assert_eq!(adlp[2].kind, IclDpllKind::Tbt);
            assert!(adlp[2].is_alt_port_dpll);
            assert_eq!(adlp[6], TC_PLL4);
        }
        assert_eq!(
            icl_dpll_descriptors(IclDpllPlatform::RocketLake),
            &[DPLL0, DPLL1, DPLL4]
        );
        assert_eq!(icl_dpll_descriptors(IclDpllPlatform::AlderLakeS).len(), 4);
        assert_eq!(icl_dpll_descriptors(IclDpllPlatform::Dg1).len(), 4);
        assert_eq!(icl_dpll_descriptors(IclDpllPlatform::ElkhartLake).len(), 3);
        assert_eq!(icl_dpll_descriptors(IclDpllPlatform::JasperLake).len(), 3);
    }

    #[test]
    fn wrpll_search_matches_source_divider_order_and_dco_window() {
        let cases = [
            (148_500, 19_200, 0x1d0, 0x0800, 1, 2, 3),
            (148_500, 24_000, 0x173, 0x2000, 1, 2, 3),
            (148_500, 38_400, 0x1d0, 0x0800, 1, 2, 3),
        ];
        for (clock, reference, integer, fraction, pdiv, kdiv, qdiv) in cases {
            let params = icl_calc_wrpll(clock, reference).unwrap();
            assert_eq!(
                (
                    params.dco_integer,
                    params.dco_fraction,
                    params.pdiv,
                    params.kdiv,
                    params.qdiv_ratio,
                ),
                (integer, fraction, pdiv, kdiv, qdiv)
            );
        }
        assert_eq!(icl_wrpll_get_multipliers(35), Err(Error::Refused));
        assert_eq!(icl_wrpll_ref_clock(38_400), Ok(19_200));
    }

    #[test]
    fn dp_and_tbt_fixed_tables_preserve_gen12_values() {
        for clock in [
            162_000, 216_000, 270_000, 324_000, 432_000, 540_000, 648_000, 810_000,
        ] {
            assert!(icl_calc_dp_combo_pll(clock, 24_000).is_ok());
            assert!(icl_calc_dp_combo_pll(clock, 38_400).is_ok());
        }
        let tbt = icl_calc_tbt_pll(12, 38_400).unwrap();
        assert_eq!(
            (tbt.dco_integer, tbt.dco_fraction, tbt.pdiv, tbt.kdiv),
            (0x54, 0x3000, 0, 0)
        );
        assert_eq!(icl_calc_tbt_pll(12, 24_000).unwrap().dco_integer, 0x43);
    }

    #[test]
    fn combo_cfgcr_encoding_readback_and_fraction_wa_round_trip() {
        let params = icl_calc_wrpll(148_500, 38_400).unwrap();
        let state = icl_calc_dpll_state(params, 12, 38_400, Some(5)).unwrap();
        assert_eq!(state.div0, 5 << 25);
        assert_eq!(state.cfgcr1 & 3, 0, "Gen12 normal-xtal override");
        assert_eq!(
            icl_ddi_combo_pll_get_freq(state, 38_400, 12, true),
            Ok(148_500)
        );
        let state = icl_calc_dpll_state(params, 11, 38_400, None).unwrap();
        assert_eq!(state.cfgcr1 & 3, 3, "Gen11 central-frequency selector");
        assert!(icl_calc_dpll_state(params, 12, 24_000, Some(8)).is_err());
    }
}
