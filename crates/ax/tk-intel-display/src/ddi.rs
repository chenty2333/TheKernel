// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_ddi.c:
// intel_ddi_read_func_ctl (selected control fields), intel_ddi_read_func_ctl_dvi,
// icl_ddi_tc_is_clock_enabled, icl_ddi_tc_get_pll, icl_calc_tbt_pll_link,
// icl_pll_to_ddi_clk_sel, ddi_buf_phy_link_rate, dp_phy_lane_stagger_delay,
// intel_ddi_init_dp_buf_reg (display12/13 branches).
// Copyright © 2012 Intel Corporation.
// intel_display_regs.h: selected fields, Copyright © 2025 Intel Corporation.
// MIT permission text: ../LICENSE-MIT. No DDI/clock programming.
use crate::{
    Error,
    device::Port,
    display::{Pipe, ReadoutIo},
    dkl_phy::TcPort,
    dpll_mgr::IclDpllId,
    tc::TcIo,
};

const DDI_CLK_SEL_MASK: u32 = 0xf << 28;
const DDI_CLK_SEL_MG: u32 = 8 << 28;
const DDI_CLK_SEL_TBT_162: u32 = 12 << 28;
const DDI_CLK_SEL_TBT_270: u32 = 13 << 28;
const DDI_CLK_SEL_TBT_540: u32 = 14 << 28;
const DDI_CLK_SEL_TBT_810: u32 = 15 << 28;

/// Decode the clock rate a TBT PLL selector carries in `DDI_CLK_SEL`.
// upstream: intel_ddi.c icl_calc_tbt_pll_link()
pub fn icl_calc_tbt_pll_link(selector: u32) -> Result<u32, Error> {
    match selector & DDI_CLK_SEL_MASK {
        0 => Ok(0),
        DDI_CLK_SEL_TBT_162 => Ok(162_000),
        DDI_CLK_SEL_TBT_270 => Ok(270_000),
        DDI_CLK_SEL_TBT_540 => Ok(540_000),
        DDI_CLK_SEL_TBT_810 => Ok(810_000),
        _ => Err(Error::Refused),
    }
}

/// Select a TBT or TC MG DPLL for a port; combo DPLL ids are not legal here.
// upstream: intel_ddi.c icl_pll_to_ddi_clk_sel()
pub fn icl_pll_to_ddi_clk_sel(pll: IclDpllId, port_clock_khz: u32) -> Result<u32, Error> {
    match pll {
        IclDpllId::Tbt => match port_clock_khz {
            162_000 => Ok(DDI_CLK_SEL_TBT_162),
            270_000 => Ok(DDI_CLK_SEL_TBT_270),
            540_000 => Ok(DDI_CLK_SEL_TBT_540),
            810_000 => Ok(DDI_CLK_SEL_TBT_810),
            _ => Err(Error::Refused),
        },
        IclDpllId::Mg1
        | IclDpllId::Mg2
        | IclDpllId::Mg3
        | IclDpllId::Mg4
        | IclDpllId::Mg5
        | IclDpllId::Mg6 => Ok(DDI_CLK_SEL_MG),
        _ => Err(Error::Refused),
    }
}

/// Map a supported DisplayPort link rate to `DDI_BUF_CTL.PHY_LINK_RATE`.
// upstream: intel_ddi.c ddi_buf_phy_link_rate()
pub fn ddi_buf_phy_link_rate(port_clock_khz: u32) -> Result<u32, Error> {
    let code = match port_clock_khz {
        162_000 => 0,
        270_000 => 1,
        540_000 => 2,
        810_000 => 3,
        216_000 => 4,
        243_000 => 5,
        324_000 => 6,
        432_000 => 7,
        _ => return Err(Error::Refused),
    };
    Ok(code << 20)
}

/// Number of DP link symbols in at least 100 ns for 8b/10b DisplayPort rates.
// upstream: intel_ddi.c dp_phy_lane_stagger_delay()
pub const fn dp_phy_lane_stagger_delay(port_clock_khz: u32) -> u32 {
    port_clock_khz.div_ceil(10_000)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DpBufferOptions {
    pub display_version: u8,
    pub alderlake_p_family: bool,
    pub lane_count: u8,
    pub lane_reversal: bool,
    pub ddi_a_4_lanes: bool,
    pub is_tc_port: bool,
    pub in_tbt_alt_mode: bool,
    pub port_clock_khz: u32,
}

/// Build `intel_dp->DP`'s DDI buffer fields before link training.
// upstream: intel_ddi.c intel_ddi_init_dp_buf_reg()
pub fn init_dp_buffer_control(options: DpBufferOptions) -> Result<u32, Error> {
    if !(1..=4).contains(&options.lane_count) {
        return Err(Error::Refused);
    }
    let width = if options.lane_count == 4 {
        4
    } else {
        u32::from(options.lane_count - 1)
    };
    let mut value = (width << 1)
        | if options.lane_reversal { 1 << 16 } else { 0 }
        | if options.ddi_a_4_lanes { 1 << 4 } else { 0 };
    if options.alderlake_p_family && options.is_tc_port {
        value |= ddi_buf_phy_link_rate(options.port_clock_khz)?;
        if !options.in_tbt_alt_mode {
            value |= 1 << 6;
        }
    }
    if (11..=13).contains(&options.display_version) && options.is_tc_port {
        let delay = dp_phy_lane_stagger_delay(options.port_clock_khz);
        if delay > 0xff {
            return Err(Error::Refused);
        }
        value |= delay << 8;
    }
    Ok(value)
}

const DDI_BUF_IS_IDLE: u32 = 1 << 7;
const DDI_BUF_IDLE_TIMEOUT_MS: u16 = 10;

/// Register backends own the poll clock and kernel's warning/logging path.
pub trait DdiBufferIo {
    fn wait_set(&self, offset: u32, mask: u32, timeout_ms: u16) -> Result<bool, Error>;
    fn wait_clear(&self, offset: u32, mask: u32, timeout_ms: u16) -> Result<bool, Error>;
    fn delay_us(&self, delay_us: u16) -> Result<(), Error>;
}

/// `intel_ddi_buf_status_reg()` for the implemented DDI-A/B block.
pub fn ddi_buf_status_offset(display_version: u8, port: Port) -> Result<u32, Error> {
    if display_version >= 14 {
        return Err(Error::Refused);
    }
    match port {
        Port::A => Ok(0x64000),
        Port::B => Ok(0x64100),
        _ => Err(Error::Refused),
    }
}

/// Wait for the DDI buffer idle bit, using the source's platform-specific
/// BXT fixed delay or the HSW–ADL wait-for-set timeout.
// upstream: intel_ddi.c intel_wait_ddi_buf_idle()
pub fn intel_wait_ddi_buf_idle(
    io: &impl DdiBufferIo,
    display_version: u8,
    port: Port,
    broxton: bool,
) -> Result<bool, Error> {
    if broxton {
        io.delay_us(16)?;
        return Ok(true);
    }
    io.wait_set(
        ddi_buf_status_offset(display_version, port)?,
        DDI_BUF_IS_IDLE,
        DDI_BUF_IDLE_TIMEOUT_MS,
    )
}

/// Wait for DDI-active (`IS_IDLE` clear); source uses a fixed HSW delay only
/// on display versions below 10 and a register poll for display-12/13.
// upstream: intel_ddi.c intel_wait_ddi_buf_active()
pub fn intel_wait_ddi_buf_active(
    io: &impl DdiBufferIo,
    display_version: u8,
    port: Port,
) -> Result<bool, Error> {
    if display_version < 10 {
        io.delay_us(518)?;
        return Ok(true);
    }
    io.wait_clear(
        ddi_buf_status_offset(display_version, port)?,
        DDI_BUF_IS_IDLE,
        DDI_BUF_IDLE_TIMEOUT_MS,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DdiTranscoderMode {
    Hdmi {
        sink_present: bool,
        scrambling: bool,
        high_tmds_ratio: bool,
    },
    Dvi,
    DisplayPortSst,
    DisplayPortMst {
        master_transcoder: Option<u8>,
    },
    Uhbr {
        master_transcoder: Option<u8>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DdiTranscoderOptions {
    pub port: Port,
    pub bits_per_pixel: u8,
    pub positive_hsync: bool,
    pub positive_vsync: bool,
    pub pipe: u8,
    pub edp: bool,
    pub edp_force_thru: bool,
    pub lane_count: u8,
    pub mode: DdiTranscoderMode,
}

/// Encode `TGL_TRANS_DDI_SELECT_PORT()`, including the source's TC aliases
/// (`PORT_TC1 == PORT_D` through `PORT_TC6 == PORT_I`).
fn tgl_transcoder_port_select(port: Port) -> Result<u32, Error> {
    let source_port = match port {
        Port::A => 0,
        Port::B => 1,
        Port::C => 2,
        Port::D | Port::Tc1 => 3,
        Port::E | Port::Tc2 => 4,
        Port::F | Port::Tc3 => 5,
        Port::Tc4 => 6,
        Port::Tc5 => 7,
        Port::Tc6 => 8,
    };
    Ok((source_port + 1) << 27)
}

/// Build the display-12/13 `TRANS_DDI_FUNC_CTL` value.
// upstream: intel_ddi.c intel_ddi_transcoder_func_reg_val_get()
pub fn transcoder_func_ctl(options: DdiTranscoderOptions) -> Result<u32, Error> {
    let mut value = (1 << 31) | tgl_transcoder_port_select(options.port)?;
    value |= match options.bits_per_pixel {
        18 => 2 << 20,
        24 => 0 << 20,
        30 => 1 << 20,
        36 => 3 << 20,
        _ => return Err(Error::Refused),
    };
    if options.positive_hsync {
        value |= 1 << 16;
    }
    if options.positive_vsync {
        value |= 1 << 17;
    }
    if options.edp {
        value |= match (options.pipe, options.edp_force_thru) {
            (0, false) => 0 << 12,
            (0, true) => 4 << 12,
            (1, _) => 5 << 12,
            (2, _) => 6 << 12,
            _ => return Err(Error::Refused),
        };
    }
    value |= match options.mode {
        DdiTranscoderMode::Hdmi {
            sink_present,
            scrambling,
            high_tmds_ratio,
        } => {
            let mut mode = (if sink_present { 0 } else { 1 }) << 24;
            if scrambling {
                mode |= 1;
            }
            if high_tmds_ratio {
                mode |= 1 << 4;
            }
            mode
        }
        DdiTranscoderMode::Dvi => 1 << 24,
        DdiTranscoderMode::DisplayPortSst => (2 << 24) | ddi_port_width(options.lane_count)?,
        DdiTranscoderMode::DisplayPortMst { master_transcoder } => {
            let master = master_transcoder.unwrap_or(0);
            if master > 3 {
                return Err(Error::Refused);
            }
            let width = ddi_port_width(options.lane_count)?;
            (3 << 24) | width | (u32::from(master) << 10)
        }
        DdiTranscoderMode::Uhbr { master_transcoder } => {
            let master = master_transcoder.unwrap_or(0);
            if master > 3 {
                return Err(Error::Refused);
            }
            let width = ddi_port_width(options.lane_count)?;
            (4 << 24) | width | (u32::from(master) << 10)
        }
    };
    Ok(value)
}

/// Configure function-control fields with the enable bit clear, as used during
/// the pre-enable stage before transcoder activation.
// upstream: intel_ddi.c intel_ddi_config_transcoder_func()
pub fn transcoder_config_ctl(options: DdiTranscoderOptions) -> Result<u32, Error> {
    Ok(transcoder_func_ctl(options)? & !(1 << 31))
}

fn ddi_port_width(lane_count: u8) -> Result<u32, Error> {
    match lane_count {
        1..=4 => Ok(u32::from(lane_count - 1) << 1),
        _ => Err(Error::Refused),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DdiClockPlatform {
    Icl,
    TigerLake,
    RocketLake,
    AlderLakeS,
    AlderLakeP,
    AlderLakeN,
    Dg1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DdiClockPlan {
    pub register: u32,
    pub selector_mask: u32,
    pub selector_value: u32,
    pub clock_off_mask: u32,
}

/// The two distinct RMWs `_icl_ddi_enable_clock()` performs under the shared
/// DPLL lock: first select the PLL, then clear the DDI clock-off gate.
pub const fn ddi_clock_enable_writes(plan: DdiClockPlan) -> [(u32, u32, u32); 2] {
    [
        (plan.register, plan.selector_mask, plan.selector_value),
        (plan.register, plan.clock_off_mask, 0),
    ]
}

/// One RMW to gate the DDI port clock, matching `_icl_ddi_disable_clock()`.
pub const fn ddi_clock_disable_write(plan: DdiClockPlan) -> (u32, u32, u32) {
    (plan.register, 0, plan.clock_off_mask)
}

/// Construct the platform-specific DPCLKA register and field layout.
// upstream: intel_ddi.c adls_ddi_get_pll()/rkl_ddi_get_pll()/dg1_ddi_get_pll()/icl_ddi_combo_get_pll()
pub fn ddi_combo_clock_plan(
    platform: DdiClockPlatform,
    phy: u8,
    pll_id: u8,
) -> Result<DdiClockPlan, Error> {
    if phy > 4 {
        return Err(Error::Refused);
    }
    let (register, shift, clock_off_bit, encoded_pll) = match platform {
        DdiClockPlatform::Icl
        | DdiClockPlatform::TigerLake
        | DdiClockPlatform::AlderLakeP
        | DdiClockPlatform::AlderLakeN => {
            if pll_id > 1 {
                return Err(Error::Refused);
            }
            (0x164280, phy * 2, icl_ddi_clock_off_bit(phy)?, pll_id)
        }
        DdiClockPlatform::RocketLake => {
            if phy > 3 || pll_id > 2 {
                return Err(Error::Refused);
            }
            let shift = match phy {
                0 => 0,
                1 => 2,
                2 => 4,
                3 => 27,
                _ => return Err(Error::Refused),
            };
            (0x164280, shift, icl_ddi_clock_off_bit(phy)?, pll_id)
        }
        DdiClockPlatform::AlderLakeS => {
            if pll_id > 3 {
                return Err(Error::Refused);
            }
            (
                if phy < 3 { 0x164280 } else { 0x1642bc },
                (phy % 3) * 2,
                icl_ddi_clock_off_bit(phy)?,
                pll_id,
            )
        }
        DdiClockPlatform::Dg1 => {
            if phy > 3 || pll_id > 3 {
                return Err(Error::Refused);
            }
            let local_phy = phy % 2;
            (
                if phy < 2 { 0x164280 } else { 0x16c280 },
                local_phy * 2,
                (1 << (local_phy + 10)),
                pll_id % 2,
            )
        }
    };
    let selector_mask = 3 << shift;
    Ok(DdiClockPlan {
        register,
        selector_mask,
        selector_value: u32::from(encoded_pll) << shift,
        clock_off_mask: clock_off_bit,
    })
}

fn icl_ddi_clock_off_bit(phy: u8) -> Result<u32, Error> {
    match phy {
        0 => Ok(1 << 10),
        1 => Ok(1 << 11),
        2 => Ok(1 << 24),
        3 => Ok(1 << 4),
        4 => Ok(1 << 5),
        _ => Err(Error::Refused),
    }
}

/// Decode the selected PLL ID from a DPCLKA register image.
// upstream: intel_ddi.c _icl_ddi_get_pll()/dg1_ddi_get_pll()
pub fn ddi_combo_clock_pll_id(platform: DdiClockPlatform, phy: u8, raw: u32) -> Result<u8, Error> {
    let plan = ddi_combo_clock_plan(platform, phy, 0)?;
    let encoded = ((raw & plan.selector_mask) >> plan.selector_mask.trailing_zeros()) as u8;
    Ok(if platform == DdiClockPlatform::Dg1 && phy >= 2 {
        encoded + 2
    } else {
        encoded
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DdiMode {
    Hdmi,
    Dvi,
    DisplayPortSst,
    DisplayPortMst,
    Fdi,
    Unknown(u8),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DdiFunction {
    pub raw: u32,
    pub enabled: bool,
    pub port: Option<Port>,
    pub mode: DdiMode,
    pub bpp: Option<u32>,
    pub positive_hsync: bool,
    pub positive_vsync: bool,
    pub hdmi_scrambling: bool,
    pub high_tmds_ratio: bool,
    pub port_sync: bool,
    pub hdmi_lanes: u32,
}
pub fn decode_function_control(raw: u32) -> DdiFunction {
    let port = match (raw >> 27) & 15 {
        1 => Some(Port::A),
        2 => Some(Port::B),
        4 => Some(Port::Tc1),
        5 => Some(Port::Tc2),
        6 => Some(Port::Tc3),
        7 => Some(Port::Tc4),
        _ => None,
    };
    let mode = match (raw >> 24) & 7 {
        0 => DdiMode::Hdmi,
        1 => DdiMode::Dvi,
        2 => DdiMode::DisplayPortSst,
        3 => DdiMode::DisplayPortMst,
        4 => DdiMode::Fdi,
        v => DdiMode::Unknown(v as u8),
    };
    let bpp = match (raw >> 20) & 7 {
        0 => Some(24),
        1 => Some(30),
        2 => Some(18),
        3 => Some(36),
        _ => None,
    };
    DdiFunction {
        raw,
        enabled: raw & (1 << 31) != 0,
        port,
        mode,
        bpp,
        positive_hsync: raw & (1 << 16) != 0,
        positive_vsync: raw & (1 << 17) != 0,
        hdmi_scrambling: matches!(mode, DdiMode::Hdmi) && raw & 1 != 0,
        high_tmds_ratio: matches!(mode, DdiMode::Hdmi) && raw & (1 << 4) != 0,
        port_sync: raw & (1 << 15) != 0,
        // i915 display<14 DVI/HDMI always has four lanes; the DP width field
        // must not be used to derive the HDMI lane count.
        hdmi_lanes: if matches!(mode, DdiMode::Hdmi | DdiMode::Dvi) {
            4
        } else {
            0
        },
    }
}
pub fn read_function_control(io: &impl ReadoutIo, pipe: Pipe) -> Result<DdiFunction, Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    Ok(decode_function_control(
        io.read32(pipe.transcoder_register(0x60400))?,
    ))
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcPllKind {
    Dkl,
    Tbt,
    Disabled,
    Unknown(u8),
}
pub fn tc_pll_kind(raw: u32) -> TcPllKind {
    match raw >> 28 {
        0 => TcPllKind::Disabled,
        8 => TcPllKind::Dkl,
        12..=15 => TcPllKind::Tbt,
        v => TcPllKind::Unknown(v as u8),
    }
}
pub fn tc_clock_select_register(port: TcPort) -> u32 {
    0x4610c + port.index() * 4
}
pub fn tc_clock_off_mask(port: TcPort) -> u32 {
    1 << if port.index() < 3 {
        12 + port.index()
    } else {
        21
    }
}
fn check_power(io: &impl TcIo, port: TcPort) -> Result<(), Error> {
    if !io.display_core_powered() || !io.tc_port_powered(port) {
        Err(Error::Refused)
    } else {
        Ok(())
    }
}
pub fn icl_ddi_tc_is_clock_enabled(io: &impl TcIo, port: TcPort) -> Result<bool, Error> {
    check_power(io, port)?;
    let select = io.read32(tc_clock_select_register(port))?;
    if select & 0xf0000000 == 0 {
        return Ok(false);
    }
    Ok(io.read32(0x164280)? & tc_clock_off_mask(port) == 0)
}
pub fn icl_ddi_tc_get_pll(io: &impl TcIo, port: TcPort) -> Result<Option<TcPllKind>, Error> {
    check_power(io, port)?;
    let kind = tc_pll_kind(io.read32(tc_clock_select_register(port))?);
    // Source warns and returns NULL for unknown selection. Do not quietly
    // reinterpret a combo/TBT selector as the local TC DKL PLL.
    Ok(match kind {
        TcPllKind::Dkl | TcPllKind::Tbt => Some(kind),
        _ => None,
    })
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcClockState {
    pub select: u32,
    pub dpclka: u32,
    pub pll: TcPllKind,
    pub enabled: bool,
}
/// Original combined evidence for equivalence/recovery, without duplicate
/// selector reads. Inactive clock selection still retains the global gate word.
pub fn read_tc_clock_state(io: &impl TcIo, port: TcPort) -> Result<TcClockState, Error> {
    check_power(io, port)?;
    let select = io.read32(tc_clock_select_register(port))?;
    let dpclka = io.read32(0x164280)?;
    let pll = tc_pll_kind(select);
    Ok(TcClockState {
        select,
        dpclka,
        pll,
        enabled: select & 0xf0000000 != 0 && dpclka & tc_clock_off_mask(port) == 0,
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    extern crate std;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct WaitModel {
        operations: Mutex<Vec<(u8, u32, u32, u16)>>,
    }
    impl DdiBufferIo for WaitModel {
        fn wait_set(&self, offset: u32, mask: u32, timeout_ms: u16) -> Result<bool, Error> {
            self.operations
                .lock()
                .unwrap()
                .push((0, offset, mask, timeout_ms));
            Ok(true)
        }
        fn wait_clear(&self, offset: u32, mask: u32, timeout_ms: u16) -> Result<bool, Error> {
            self.operations
                .lock()
                .unwrap()
                .push((1, offset, mask, timeout_ms));
            Ok(true)
        }
        fn delay_us(&self, delay_us: u16) -> Result<(), Error> {
            self.operations.lock().unwrap().push((2, 0, 0, delay_us));
            Ok(())
        }
    }

    #[test]
    fn tbt_and_mg_clock_select_encodings_match_i915_fields() {
        for (clock, code) in [(162_000, 12), (270_000, 13), (540_000, 14), (810_000, 15)] {
            let select = icl_pll_to_ddi_clk_sel(IclDpllId::Tbt, clock).unwrap();
            assert_eq!(select, code << 28);
            assert_eq!(icl_calc_tbt_pll_link(select), Ok(clock));
        }
        assert_eq!(icl_pll_to_ddi_clk_sel(IclDpllId::Mg2, 540_000), Ok(8 << 28));
        assert!(icl_pll_to_ddi_clk_sel(IclDpllId::Dpll0, 540_000).is_err());
        assert!(icl_pll_to_ddi_clk_sel(IclDpllId::Tbt, 300_000).is_err());
        assert!(icl_calc_tbt_pll_link(3 << 28).is_err());
    }

    #[test]
    fn dp_buffer_setup_translates_rate_ownership_reversal_and_stagger() {
        let value = init_dp_buffer_control(DpBufferOptions {
            display_version: 13,
            alderlake_p_family: true,
            lane_count: 4,
            lane_reversal: true,
            ddi_a_4_lanes: true,
            is_tc_port: true,
            in_tbt_alt_mode: false,
            port_clock_khz: 540_000,
        })
        .unwrap();
        assert_eq!(value, 0x0021_3658);

        let alt = init_dp_buffer_control(DpBufferOptions {
            in_tbt_alt_mode: true,
            ..DpBufferOptions {
                display_version: 13,
                alderlake_p_family: true,
                lane_count: 4,
                lane_reversal: false,
                ddi_a_4_lanes: false,
                is_tc_port: true,
                in_tbt_alt_mode: false,
                port_clock_khz: 162_000,
            }
        })
        .unwrap();
        assert_eq!(alt & (1 << 6), 0, "TBT alt mode owns the PHY elsewhere");
        assert_eq!((alt >> 20) & 0xf, 0);
        assert_eq!((alt >> 8) & 0xff, 17);
        assert_eq!(dp_phy_lane_stagger_delay(540_000), 54);
    }

    #[test]
    fn ddi_buffer_waits_use_idle_polarity_and_display12_poll_path() {
        let io = WaitModel::default();
        assert!(intel_wait_ddi_buf_idle(&io, 13, Port::A, false).unwrap());
        assert!(intel_wait_ddi_buf_active(&io, 13, Port::B).unwrap());
        assert!(intel_wait_ddi_buf_idle(&io, 9, Port::A, true).unwrap());
        assert_eq!(
            *io.operations.lock().unwrap(),
            [
                (0, 0x64000, DDI_BUF_IS_IDLE, DDI_BUF_IDLE_TIMEOUT_MS),
                (1, 0x64100, DDI_BUF_IS_IDLE, DDI_BUF_IDLE_TIMEOUT_MS),
                (2, 0, 0, 16),
            ]
        );
        assert!(ddi_buf_status_offset(13, Port::Tc1).is_err());
        assert!(ddi_buf_status_offset(14, Port::A).is_err());
    }

    #[test]
    fn transcoder_func_ctl_matches_display12_port_bpc_sync_and_dp_modes() {
        let options = DdiTranscoderOptions {
            port: Port::Tc1,
            bits_per_pixel: 24,
            positive_hsync: true,
            positive_vsync: true,
            pipe: 0,
            edp: false,
            edp_force_thru: false,
            lane_count: 4,
            mode: DdiTranscoderMode::DisplayPortSst,
        };
        assert_eq!(transcoder_func_ctl(options), Ok(0xa203_0006));
        assert_eq!(transcoder_config_ctl(options), Ok(0x2203_0006));
        assert_eq!(
            transcoder_func_ctl(DdiTranscoderOptions {
                mode: DdiTranscoderMode::DisplayPortMst {
                    master_transcoder: Some(2)
                },
                ..options
            })
            .unwrap()
                & (3 << 24 | 3 << 10 | 3 << 1),
            (3 << 24) | (2 << 10) | (3 << 1)
        );
        assert_eq!(
            transcoder_func_ctl(DdiTranscoderOptions {
                port: Port::A,
                bits_per_pixel: 30,
                lane_count: 0,
                positive_hsync: false,
                positive_vsync: false,
                mode: DdiTranscoderMode::Hdmi {
                    sink_present: false,
                    scrambling: false,
                    high_tmds_ratio: false,
                },
                ..options
            }),
            Ok(0x8910_0000)
        );
        assert!(
            transcoder_func_ctl(DdiTranscoderOptions {
                bits_per_pixel: 16,
                ..options
            })
            .is_err()
        );
    }

    #[test]
    fn combo_phy_clock_mux_uses_platform_registers_and_two_write_order() {
        assert_eq!(
            ddi_combo_clock_plan(DdiClockPlatform::AlderLakeN, 0, 0).unwrap(),
            ddi_combo_clock_plan(DdiClockPlatform::Icl, 0, 0).unwrap()
        );
        let icl = ddi_combo_clock_plan(DdiClockPlatform::TigerLake, 1, 1).unwrap();
        assert_eq!(
            icl,
            DdiClockPlan {
                register: 0x164280,
                selector_mask: 3 << 2,
                selector_value: 1 << 2,
                clock_off_mask: 1 << 11,
            }
        );
        assert_eq!(
            ddi_clock_enable_writes(icl),
            [(0x164280, 3 << 2, 1 << 2), (0x164280, 1 << 11, 0)]
        );
        assert_eq!(ddi_clock_disable_write(icl), (0x164280, 0, 1 << 11));
        assert_eq!(
            ddi_combo_clock_pll_id(DdiClockPlatform::TigerLake, 1, 1 << 2),
            Ok(1)
        );

        let adls = ddi_combo_clock_plan(DdiClockPlatform::AlderLakeS, 3, 3).unwrap();
        assert_eq!(
            adls,
            DdiClockPlan {
                register: 0x1642bc,
                selector_mask: 3,
                selector_value: 3,
                clock_off_mask: 1 << 4,
            }
        );
        let rkl = ddi_combo_clock_plan(DdiClockPlatform::RocketLake, 3, 2).unwrap();
        assert_eq!(rkl.selector_value, 2 << 27);
        let dg1 = ddi_combo_clock_plan(DdiClockPlatform::Dg1, 2, 3).unwrap();
        assert_eq!(
            (dg1.register, dg1.selector_value, dg1.clock_off_mask),
            (0x16c280, 1, 1 << 10)
        );
        assert_eq!(ddi_combo_clock_pll_id(DdiClockPlatform::Dg1, 2, 1), Ok(3));
        assert!(ddi_combo_clock_plan(DdiClockPlatform::RocketLake, 4, 0).is_err());
    }
}
