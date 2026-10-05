// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_tc.c:
// adlp_tc_phy_is_ready, adlp_tc_phy_is_owned, tc_phy_owned_by_display,
// tc_phy_load_fia_params, get_pin_assignment (display13 modular FIA branch).
// Copyright © 2019 Intel Corporation.
// intel_display_regs.h / intel_{mg,dkl}_phy_regs.h: selected register layout,
// Copyright © 2025 / 2022 Intel Corporation. MIT permission text: ../LICENSE-MIT.
// This is discovery, not ownership acquisition, cold unblock or PHY connect.
use crate::{
    Error,
    dkl_phy::{DklIo, TcPort, with_preserved_selector},
};

/// Backend pins already-enabled DISPLAY_CORE and PORT_DDI_LANES_TC(n), as
/// well as the legacy AUX domain preventing TC cold, throughout these accesses.
/// None of those domains may be implicitly woken for firmware discovery.
pub trait TcIo: DklIo {
    fn display_core_powered(&self) -> bool;
    fn tc_port_powered(&self, port: TcPort) -> bool;
    fn tc_cold_blocked(&self, port: TcPort) -> bool;
}
pub fn status_register(port: TcPort) -> u32 {
    0x161500 + port.index() * 4
}
pub fn buffer_register(port: TcPort) -> u32 {
    0x64300 + port.index() * 0x100
}
pub fn adlp_tc_phy_is_ready(io: &impl TcIo, port: TcPort) -> Result<bool, Error> {
    if !io.display_core_powered() {
        return Err(Error::Refused);
    }
    let val = io.read32(status_register(port))?;
    // Source all-ones cold/unclaimed register handling, never "ready".
    Ok(val != u32::MAX && val & (1 << 2) != 0)
}
pub fn adlp_tc_phy_is_owned(io: &impl TcIo, port: TcPort) -> Result<bool, Error> {
    if !io.tc_port_powered(port) {
        return Err(Error::Refused);
    }
    let val = io.read32(buffer_register(port))?;
    Ok(val & (1 << 6) != 0)
}
pub fn tc_phy_owned_by_display(ready: bool, owned: bool) -> bool {
    ready && owned
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FiaState {
    pub pin_raw: u32,
    pub lanes_raw: u32,
    pub pin_assignment: u8,
    pub lane_mask: u8,
}
pub fn read_fia_state(io: &impl TcIo, port: TcPort) -> Result<FiaState, Error> {
    if !io.display_core_powered() || !io.tc_cold_blocked(port) {
        return Err(Error::Refused);
    }
    // ADL-P unconditionally initializes modular FIA, two ports per instance.
    let instance = port.index() / 2;
    let idx = port.index() % 2;
    let base = if instance == 0 { 0x163000 } else { 0x16e000 };
    // Do NOT use TCSS_DDI_STATUS_PIN_ASSIGNMENT_MASK: get_pin_assignment only
    // uses that field on display20+. Display13 still uses DFLEXPA1.
    let pin_raw = io.read32(base + 0x880)?;
    let lanes_raw = io.read32(base + 0x8a0)?;
    if pin_raw == u32::MAX || lanes_raw == u32::MAX {
        return Err(Error::Refused);
    }
    Ok(FiaState {
        pin_raw,
        lanes_raw,
        pin_assignment: ((pin_raw >> (idx * 4)) & 15) as u8,
        lane_mask: ((lanes_raw >> (idx * 8)) & 15) as u8,
    })
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DklLaneState {
    pub pcs_dw5: u32,
    pub dp_mode: u32,
    pub lane_suspend: u32,
    pub tx_control: [u32; 3],
    pub fw_calibration: u32,
    pub tx_dw17: u32,
    pub tx_dw18: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DklPhyState {
    pub uc_dw27: u32,
    pub lanes: [DklLaneState; 2],
}
/// Additional original before-image evidence for the exact DKL registers used
/// by upstream's HDMI signal/PHY setup. Not a claim that i915 has this getter.
/// Register layouts/accesses use the ported DKL mechanism; no PHY word is
/// written. Full HIP selector is restored/verified before returning.
pub fn read_dkl_phy_state(io: &impl TcIo, port: TcPort) -> Result<DklPhyState, Error> {
    if !io.display_core_powered()
        || !io.tc_port_powered(port)
        || !io.tc_cold_blocked(port)
        || !adlp_tc_phy_is_ready(io, port)?
        || !adlp_tc_phy_is_owned(io, port)?
    {
        return Err(Error::Refused);
    }
    with_preserved_selector(io, port, |phy| {
        let uc_dw27 = phy.read(0x236c)?;
        let mut lanes = [DklLaneState::default(); 2];
        for (n, lane) in lanes.iter_mut().enumerate() {
            let bank = n as u32 * 0x1000;
            lane.pcs_dw5 = phy.read(bank + 0x14)?;
            lane.dp_mode = phy.read(bank + 0xa0)?;
            lane.lane_suspend = phy.read(bank + 0xd00)?;
            for (n, v) in lane.tx_control.iter_mut().enumerate() {
                *v = phy.read(bank + 0x2c0 + n as u32 * 4)?;
            }
            lane.fw_calibration = phy.read(bank + 0x2f8)?;
            lane.tx_dw17 = phy.read(bank + 0xdc4)?;
            lane.tx_dw18 = phy.read(bank + 0xdc8)?;
        }
        Ok(DklPhyState { uc_dw27, lanes })
    })
}
