// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_ddi.c:
// intel_ddi_read_func_ctl (selected control fields), intel_ddi_read_func_ctl_dvi,
// icl_ddi_tc_is_clock_enabled, icl_ddi_tc_get_pll (display13 branches).
// Copyright © 2012 Intel Corporation.
// intel_display_regs.h: selected fields, Copyright © 2025 Intel Corporation.
// MIT permission text: ../LICENSE-MIT. No DDI/clock programming.
use crate::{
    Error,
    device::Port,
    display::{Pipe, ReadoutIo},
    dkl_phy::TcPort,
    tc::TcIo,
};

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
