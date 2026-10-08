// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_tc.c:
// adlp_tc_phy_is_ready, adlp_tc_phy_is_owned, tc_phy_owned_by_display,
// tc_phy_load_fia_params, get_pin_assignment (display13 modular FIA branch).
// intel_ddi.c::tgl_dkl_phy_set_signal_levels/intel_ddi_level and
// intel_ddi_buf_trans.c::_tgl_dkl_phy_trans_hdmi; intel_display_wa.c:
// Wa_16011342517 selection. Copyright © 2012, 2020, 2023 Intel Corporation.
// Copyright © 2019 Intel Corporation.
// intel_display_regs.h / intel_{mg,dkl}_phy_regs.h: selected register layout,
// Copyright © 2025 / 2022 Intel Corporation. MIT permission text: ../LICENSE-MIT.
// Discovery and already-owned HDMI signal programming only; no ownership
// acquisition, cold unblock or PHY connect.
use crate::{
    Error,
    dkl_phy::{DklIo, TcPort, with_preserved_selector},
    power_map::PowerDomain,
};

/// i915 Type-C port state, independent of the TCSS/MG PHY transport.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcPortMode {
    Disconnected,
    TbtAlt,
    DpAlt,
    Legacy,
}

/// Stable source spelling used by TC diagnostics.
// upstream: intel_tc.c tc_port_mode_name()
pub const fn tc_port_mode_name(mode: TcPortMode) -> &'static str {
    match mode {
        TcPortMode::Disconnected => "disconnected",
        TcPortMode::TbtAlt => "tbt-alt",
        TcPortMode::DpAlt => "dp-alt",
        TcPortMode::Legacy => "legacy",
    }
}

/// Whether an encoder whose platform reports it as TC is in the requested mode.
// upstream: intel_tc.c intel_tc_port_in_mode()
pub const fn intel_tc_port_in_mode(
    is_type_c: bool,
    current: TcPortMode,
    requested: TcPortMode,
) -> bool {
    is_type_c && current as u8 == requested as u8
}

/// Test the TBT alternate mode.
// upstream: intel_tc.c intel_tc_port_in_tbt_alt_mode()
pub const fn intel_tc_port_in_tbt_alt_mode(is_type_c: bool, mode: TcPortMode) -> bool {
    intel_tc_port_in_mode(is_type_c, mode, TcPortMode::TbtAlt)
}

/// Test the DisplayPort alternate mode.
// upstream: intel_tc.c intel_tc_port_in_dp_alt_mode()
pub const fn intel_tc_port_in_dp_alt_mode(is_type_c: bool, mode: TcPortMode) -> bool {
    intel_tc_port_in_mode(is_type_c, mode, TcPortMode::DpAlt)
}

/// Test legacy routing mode.
// upstream: intel_tc.c intel_tc_port_in_legacy_mode()
pub const fn intel_tc_port_in_legacy_mode(is_type_c: bool, mode: TcPortMode) -> bool {
    intel_tc_port_in_mode(is_type_c, mode, TcPortMode::Legacy)
}

/// TC ports outside legacy routing use the TC-specific HPD glitch handler.
// upstream: intel_tc.c intel_tc_port_handles_hpd_glitches()
pub const fn intel_tc_port_handles_hpd_glitches(is_type_c: bool, legacy_port: bool) -> bool {
    is_type_c && !legacy_port
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcPinAssignment {
    None,
    A,
    B,
    C,
    D,
    E,
    F,
    Unknown(u8),
}

/// Stable single-character pin-assignment diagnostic.
// upstream: intel_tc.c pin_assignment_name()
pub const fn pin_assignment_name(pin: TcPinAssignment) -> char {
    match pin {
        TcPinAssignment::None => '-',
        TcPinAssignment::A => 'A',
        TcPinAssignment::B => 'B',
        TcPinAssignment::C => 'C',
        TcPinAssignment::D => 'D',
        TcPinAssignment::E => 'E',
        TcPinAssignment::F => 'F',
        TcPinAssignment::Unknown(_) => '?',
    }
}

/// Decode the FIA pin-assignment field used by `get_pin_assignment()`.
pub const fn decode_pin_assignment(value: u8) -> TcPinAssignment {
    match value {
        0 => TcPinAssignment::None,
        1 => TcPinAssignment::A,
        2 => TcPinAssignment::B,
        3 => TcPinAssignment::C,
        4 => TcPinAssignment::D,
        5 => TcPinAssignment::E,
        6 => TcPinAssignment::F,
        n => TcPinAssignment::Unknown(n),
    }
}

/// Port configuration retained after the VBT/FIA policy read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcPortConfiguration {
    pub pin_assignment: TcPinAssignment,
    pub max_lane_count: u8,
}

/// The TBT mode has no DP pin assignment; otherwise decode the source field.
// upstream: intel_tc.c get_pin_assignment()
pub const fn get_pin_assignment(mode: TcPortMode, pin_raw: u8) -> TcPinAssignment {
    if matches!(mode, TcPortMode::TbtAlt) {
        TcPinAssignment::None
    } else {
        decode_pin_assignment(pin_raw)
    }
}

/// Recompute pin/lane configuration after a TC mode change.
// upstream: intel_tc.c read_pin_configuration()
pub const fn read_pin_configuration(
    mode: TcPortMode,
    display_version: u8,
    lane_mask: u8,
    pin_raw: u8,
) -> TcPortConfiguration {
    let pin_assignment = get_pin_assignment(mode, pin_raw);
    let max_lane_count = get_max_lane_count(mode, display_version, lane_mask, pin_assignment);
    TcPortConfiguration {
        pin_assignment,
        max_lane_count,
    }
}

/// Non-Type-C encoders report no TC pin-assignment value.
// upstream: intel_tc.c intel_tc_port_get_pin_assignment()
pub const fn intel_tc_port_get_pin_assignment(
    is_type_c: bool,
    pin: TcPinAssignment,
) -> TcPinAssignment {
    if is_type_c {
        pin
    } else {
        TcPinAssignment::None
    }
}

/// Translate the FIA DP lane mask to the maximum source lane count.
// upstream: intel_tc.c icl_get_max_lane_count()
pub const fn icl_get_max_lane_count(lane_mask: u8) -> u8 {
    match lane_mask {
        0x1 | 0x2 | 0x4 | 0x8 => 1,
        0x3 | 0xc => 2,
        0xf => 4,
        _ => 1, // MISSING_CASE then fallthrough to the source's one-lane cases
    }
}

/// Translate the newer pin-assignment table to a maximum lane count.
// upstream: intel_tc.c mtl_get_max_lane_count()
pub const fn mtl_get_max_lane_count(pin: TcPinAssignment) -> u8 {
    match pin {
        TcPinAssignment::None => 0,
        TcPinAssignment::C | TcPinAssignment::E => 4,
        TcPinAssignment::D | TcPinAssignment::Unknown(_) => 2,
        TcPinAssignment::A | TcPinAssignment::B | TcPinAssignment::F => 2,
    }
}

/// Display-version/mode dispatch used after reading the port's lane topology.
// upstream: intel_tc.c get_max_lane_count()
pub const fn get_max_lane_count(
    mode: TcPortMode,
    display_version: u8,
    lane_mask: u8,
    pin: TcPinAssignment,
) -> u8 {
    if !matches!(mode, TcPortMode::DpAlt) {
        4
    } else if display_version >= 14 {
        mtl_get_max_lane_count(pin)
    } else {
        icl_get_max_lane_count(lane_mask)
    }
}

/// Derive the TC lane power domain from TC1's base domain.
// upstream: intel_tc.c tc_port_power_domain()
pub const fn tc_port_power_domain(port: TcPort) -> PowerDomain {
    match port {
        TcPort::Tc1 => PowerDomain::PortDdiLanesTc1,
        TcPort::Tc2 => PowerDomain::PortDdiLanesTc2,
        TcPort::Tc3 => PowerDomain::PortDdiLanesTc3,
        TcPort::Tc4 => PowerDomain::PortDdiLanesTc4,
    }
}

/// Whether the current TC cold-off domain is the port's legacy AUX domain.
// upstream: intel_tc.c intel_tc_cold_requires_aux_pw()
pub const fn intel_tc_cold_requires_aux_pw(
    cold_off_domain: PowerDomain,
    legacy_aux_domain: PowerDomain,
) -> bool {
    matches!((cold_off_domain, legacy_aux_domain), (a, b) if a as u16 == b as u16)
}

/// FIA number and within-FIA port index selected for one TC port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FiaParams {
    pub fia_index: u8,
    pub port_index: u8,
}

/// Load FIA topology according to the modular-FIA platform bit.
// upstream: intel_tc.c tc_phy_load_fia_params()
pub const fn tc_phy_load_fia_params(tc_port_index: u8, modular_fia: bool) -> FiaParams {
    if modular_fia {
        FiaParams {
            fia_index: tc_port_index / 2,
            port_index: tc_port_index % 2,
        }
    } else {
        FiaParams {
            fia_index: 0,
            port_index: tc_port_index,
        }
    }
}

/// Correct a VBT legacy-port flag when the live status has one other mode.
// upstream: intel_tc.c tc_port_fixup_legacy_flag()
pub const fn tc_port_fixup_legacy_flag(legacy_port: bool, live_status_mask: u32) -> bool {
    if live_status_mask.count_ones() != 1 {
        legacy_port
    } else {
        let expected = if legacy_port {
            1 << TcPortMode::Legacy as u8
        } else {
            (1 << TcPortMode::DpAlt as u8) | (1 << TcPortMode::TbtAlt as u8)
        };
        if live_status_mask & !expected == 0 {
            legacy_port
        } else {
            !legacy_port
        }
    }
}

/// The public connector query returns four lanes on a non-Type-C encoder.
// upstream: intel_tc.c intel_tc_port_max_lane_count()
pub const fn intel_tc_port_max_lane_count(is_type_c: bool, lane_count: u8) -> u8 {
    if is_type_c { lane_count } else { 4 }
}

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
    let fia = tc_phy_load_fia_params(port.index() as u8, true);
    let base = if fia.fia_index == 0 {
        0x163000
    } else {
        0x16e000
    };
    let idx = u32::from(fia.port_index);
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

/// Exact known stepping decision for display Wa_16011342517. `Unknown` is
/// rejected before PHY access; callers must derive this from an exact PCI
/// display-stepping match, never infer it from an active firmware link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wa16011342517 {
    Active,
    Inactive,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DklHdmiEntry {
    vswing: u8,
    preshoot: u8,
    de_emphasis: u8,
}

// Linux 7.2.3 intel_ddi_buf_trans.c::_tgl_dkl_phy_trans_hdmi; index is the
// sanitized VBT level-shifter value (0-based in intel_ddi_level), not preset
// label minus one. The N305 VBT selects index 5.
const ADLP_DKL_HDMI_TRANS: [DklHdmiEntry; 10] = [
    DklHdmiEntry {
        vswing: 7,
        preshoot: 0,
        de_emphasis: 0,
    },
    DklHdmiEntry {
        vswing: 6,
        preshoot: 0,
        de_emphasis: 0,
    },
    DklHdmiEntry {
        vswing: 4,
        preshoot: 0,
        de_emphasis: 0,
    },
    DklHdmiEntry {
        vswing: 2,
        preshoot: 0,
        de_emphasis: 0,
    },
    DklHdmiEntry {
        vswing: 0,
        preshoot: 0,
        de_emphasis: 0,
    },
    DklHdmiEntry {
        vswing: 0,
        preshoot: 0,
        de_emphasis: 5,
    },
    DklHdmiEntry {
        vswing: 0,
        preshoot: 0,
        de_emphasis: 6,
    },
    DklHdmiEntry {
        vswing: 0,
        preshoot: 0,
        de_emphasis: 7,
    },
    DklHdmiEntry {
        vswing: 0,
        preshoot: 0,
        de_emphasis: 8,
    },
    DklHdmiEntry {
        vswing: 0,
        preshoot: 0,
        de_emphasis: 10,
    },
];

const DKL_TX_PRESHOOT_MASK: u32 = 0x1f << 13;
const DKL_TX_DE_EMPHASIS_MASK: u32 = 0x1f << 8;
const DKL_TX_VSWING_MASK: u32 = 0x7;
const DKL_TX_DPCNTL2_DP20BITMODE: u32 = 1 << 2;
const DKL_TX_DPCNTL2_LOADGEN_TX1_MASK: u32 = 0x3 << 3;
const DKL_TX_DPCNTL2_LOADGEN_TX2_MASK: u32 = 0x3 << 5;
const DKL_TX_DPCNTL2_WA_16011342517: u32 = 1 << 12;

#[derive(Clone, Copy)]
struct DklHdmiPolicy {
    signal_control: u32,
    wa_set: Option<u32>,
}

impl DklHdmiPolicy {
    fn for_mode(
        port_clock_khz: u32,
        hdmi_level_shift: u8,
        wa_16011342517: Wa16011342517,
    ) -> Result<Self, Error> {
        if !(25_000..=594_000).contains(&port_clock_khz)
            || usize::from(hdmi_level_shift) >= ADLP_DKL_HDMI_TRANS.len()
            || wa_16011342517 == Wa16011342517::Unknown
        {
            return Err(Error::Refused);
        }
        let trans = ADLP_DKL_HDMI_TRANS[usize::from(hdmi_level_shift)];
        Ok(Self {
            signal_control: (u32::from(trans.preshoot) << 13)
                | (u32::from(trans.de_emphasis) << 8)
                | u32::from(trans.vswing),
            // Match intel_ddi.c exactly: at 594 MHz set literal 1, otherwise
            // literal 0, after clearing bit 12. `1` means bit 0, not bit 12.
            wa_set: (wa_16011342517 == Wa16011342517::Active)
                .then_some(if port_clock_khz == 594_000 { 1 } else { 0 }),
        })
    }

    fn update_signal(self, old: u32) -> u32 {
        (old & !(DKL_TX_PRESHOOT_MASK | DKL_TX_DE_EMPHASIS_MASK | DKL_TX_VSWING_MASK))
            | self.signal_control
    }

    fn update_dpcntl2(self, mut old: u32, lane: usize) -> u32 {
        if let Some(set) = self.wa_set {
            old = (old & !DKL_TX_DPCNTL2_WA_16011342517) | set;
        }
        old &= !DKL_TX_DPCNTL2_DP20BITMODE;
        let (tx1, tx2) = if lane == 0 { (0, 2) } else { (3, 3) };
        (old & !(DKL_TX_DPCNTL2_LOADGEN_TX1_MASK | DKL_TX_DPCNTL2_LOADGEN_TX2_MASK))
            | (tx1 << 3)
            | (tx2 << 5)
    }
}

/// Return the exact DKL PHY words that Linux's ADL-P HDMI signal setup would
/// leave after programming this mode. Unrelated before-image fields are
/// preserved. The programming helper and this oracle share the same validated
/// VBT-table, workaround, and RMW policy.
pub fn adlp_tc_dkl_hdmi_expected_phy_state(
    before: DklPhyState,
    port: TcPort,
    port_clock_khz: u32,
    hdmi_level_shift: u8,
    wa_16011342517: Wa16011342517,
) -> Result<DklPhyState, Error> {
    if !matches!(port, TcPort::Tc1 | TcPort::Tc2) {
        return Err(Error::Refused);
    }
    let policy = DklHdmiPolicy::for_mode(port_clock_khz, hdmi_level_shift, wa_16011342517)?;
    let mut expected = before;
    for (lane_index, lane) in expected.lanes.iter_mut().enumerate() {
        lane.lane_suspend = 0;
        lane.tx_control[0] = policy.update_signal(lane.tx_control[0]);
        lane.tx_control[1] = policy.update_signal(lane.tx_control[1]);
        lane.tx_control[2] = policy.update_dpcntl2(lane.tx_control[2], lane_index);
    }
    Ok(expected)
}

struct DklWriteAccess<'a, I> {
    io: &'a I,
    port: TcPort,
}

impl<I: DklIo> DklWriteAccess<'_, I> {
    fn select(&self, internal: u32) -> Result<crate::dkl_phy::DklRegister, Error> {
        crate::dkl_phy::DklRegister::new(self.port, internal)
    }

    fn read(&self, internal: u32) -> Result<u32, Error> {
        let reg = self.select(internal)?;
        self.io.write32(reg.selector(), reg.index_value())?;
        let value = self.io.read32(reg.aperture())?;
        if value == u32::MAX {
            return Err(Error::Unavailable(reg.aperture()));
        }
        Ok(value)
    }

    fn write(&self, internal: u32, value: u32) -> Result<(), Error> {
        let reg = self.select(internal)?;
        self.io.write32(reg.selector(), reg.index_value())?;
        self.io.write32(reg.aperture(), value)
    }

    fn rmw(&self, internal: u32, clear: u32, set: u32) -> Result<(), Error> {
        let reg = self.select(internal)?;
        self.io.write32(reg.selector(), reg.index_value())?;
        let old = self.io.read32(reg.aperture())?;
        if old == u32::MAX {
            return Err(Error::Unavailable(reg.aperture()));
        }
        // Match intel_de_rmw/intel_uncore_rmw: source always writes the
        // read-modify-write result, even when the selected bits were equal.
        self.io.write32(reg.aperture(), (old & !clear) | set)
    }
}

/// Program the ADL-P/N legacy HDMI DKL TX settings selected by i915 for a
/// validated HDMI port clock. The caller must already own the active TC PHY,
/// hold its display power wells, and include these PHY writes in the enclosing
/// modeset before-image/rollback transaction. This does not acquire ownership,
/// leave TC-cold, change the PLL, or handle TBT/DP.
///
/// Signal level is the VBT HDMI level-shifter value used by
/// `intel_ddi_level()` (0..10 table indexes); the N305 VBT's value 5 maps to
/// the source DKL entry { VSwing=0, pre-shoot=0, de-emphasis=5 }.
pub fn adlp_tc_dkl_hdmi_set_signal_levels(
    io: &impl TcIo,
    port: TcPort,
    port_clock_khz: u32,
    hdmi_level_shift: u8,
    wa_16011342517: Wa16011342517,
) -> Result<(), Error> {
    if !matches!(port, TcPort::Tc1 | TcPort::Tc2) {
        return Err(Error::Refused);
    }
    let policy = DklHdmiPolicy::for_mode(port_clock_khz, hdmi_level_shift, wa_16011342517)?;
    if !io.display_core_powered()
        || !io.tc_port_powered(port)
        || !io.tc_cold_blocked(port)
        || !adlp_tc_phy_is_ready(io, port)?
        || !adlp_tc_phy_is_owned(io, port)?
    {
        return Err(Error::Refused);
    }

    io.with_dkl_lock(|| {
        let selector = 0x1010a0;
        let before = io.read32(selector)?;
        if before == u32::MAX {
            return Err(Error::Unavailable(selector));
        }
        let result = (|| {
            let phy = DklWriteAccess { io, port };
            for lane in 0..2u32 {
                let bank = lane * 0x1000;

                // Source Wa_16011342517: note the literal set value 1 after
                // clearing the named bit-12 mask. Keep this source behavior
                // exactly; it is not equivalent to setting bit 12.
                if let Some(set) = policy.wa_set {
                    phy.rmw(bank + 0x2c8, DKL_TX_DPCNTL2_WA_16011342517, set)?;
                }

                // Source writes lane-suspend to zero, then updates the two
                // TX controls and DPCNTL2 fields without disturbing other bits.
                phy.write(bank + 0x0d00, 0)?;
                phy.rmw(
                    bank + 0x2c0,
                    DKL_TX_PRESHOOT_MASK | DKL_TX_DE_EMPHASIS_MASK | DKL_TX_VSWING_MASK,
                    policy.signal_control,
                )?;
                phy.rmw(
                    bank + 0x2c4,
                    DKL_TX_PRESHOOT_MASK | DKL_TX_DE_EMPHASIS_MASK | DKL_TX_VSWING_MASK,
                    policy.signal_control,
                )?;
                phy.rmw(bank + 0x2c8, DKL_TX_DPCNTL2_DP20BITMODE, 0)?;

                let (tx1, tx2) = if lane == 0 { (0, 2) } else { (3, 3) };
                phy.rmw(
                    bank + 0x2c8,
                    DKL_TX_DPCNTL2_LOADGEN_TX1_MASK | DKL_TX_DPCNTL2_LOADGEN_TX2_MASK,
                    (tx1 << 3) | (tx2 << 5),
                )?;
            }
            Ok(())
        })();

        // i915 leaves the HIP index at the last PHY register. This adapter
        // restores the shared selector, including after a failed prefix.
        if io.write32(selector, before).is_err() || io.read32(selector).ok() != Some(before) {
            return Err(Error::RestoreFailed(selector));
        }
        result
    })
}

#[cfg(test)]
mod signal_level_tests {
    extern crate std;

    use std::{collections::BTreeMap, sync::Mutex};

    use super::*;
    use crate::RegisterIo;

    #[derive(Default)]
    struct State {
        mmio: BTreeMap<u32, u32>,
        dkl: BTreeMap<(u32, u32), u32>,
        dkl_writes: std::vec::Vec<(u32, u32, u32)>,
        fail_write_once: Option<u32>,
    }

    struct Model {
        state: Mutex<State>,
        dkl_lock: Mutex<()>,
    }

    impl Model {
        fn new(port: TcPort, hip: u32) -> Self {
            let mut state = State::default();
            state.mmio.insert(0x1010a0, hip);
            state.mmio.insert(status_register(port), 1 << 2);
            state.mmio.insert(buffer_register(port), 1 << 6);
            for lane in 0..2u32 {
                let bank = lane * 0x1000;
                state.dkl.insert((port.index(), bank + 0x2c0), 0xa5a5_5a5a);
                state.dkl.insert((port.index(), bank + 0x2c4), 0x5a5a_a5a5);
                state.dkl.insert((port.index(), bank + 0x2c8), 0x8123_45ff);
                state.dkl.insert((port.index(), bank + 0x0d00), 0x1234_5678);
            }
            Self {
                state: Mutex::new(state),
                dkl_lock: Mutex::new(()),
            }
        }

        fn value(&self, port: TcPort, internal: u32) -> u32 {
            *self
                .state
                .lock()
                .unwrap()
                .dkl
                .get(&(port.index(), internal))
                .unwrap()
        }

        fn dkl_writes(&self) -> std::vec::Vec<(u32, u32, u32)> {
            self.state.lock().unwrap().dkl_writes.clone()
        }

        fn selector(&self) -> u32 {
            *self.state.lock().unwrap().mmio.get(&0x1010a0).unwrap()
        }

        fn set_fail_write_once(&self, offset: u32) {
            self.state.lock().unwrap().fail_write_once = Some(offset);
        }

        fn dkl_address(offset: u32) -> Option<(u32, u32)> {
            let relative = offset.checked_sub(0x168000)?;
            let port = relative / 0x1000;
            if port >= 4 {
                return None;
            }
            Some((port, relative & 0xfff))
        }
    }

    impl RegisterIo for Model {
        fn read32(&self, offset: u32) -> Result<u32, Error> {
            let state = self.state.lock().unwrap();
            if let Some((port, low)) = Self::dkl_address(offset) {
                let selector = *state.mmio.get(&0x1010a0).unwrap_or(&u32::MAX);
                let bank = (selector >> (8 * port)) & 0xf;
                return Ok(*state
                    .dkl
                    .get(&(port, (bank << 12) | low))
                    .unwrap_or(&u32::MAX));
            }
            Ok(*state.mmio.get(&offset).unwrap_or(&u32::MAX))
        }

        fn write32(&self, offset: u32, value: u32) -> Result<(), Error> {
            let mut state = self.state.lock().unwrap();
            if state.fail_write_once == Some(offset) {
                state.fail_write_once = None;
                return Err(Error::Unavailable(offset));
            }
            if let Some((port, low)) = Self::dkl_address(offset) {
                let selector = *state.mmio.get(&0x1010a0).unwrap_or(&u32::MAX);
                let bank = (selector >> (8 * port)) & 0xf;
                let internal = (bank << 12) | low;
                state.dkl.insert((port, internal), value);
                state.dkl_writes.push((port, internal, value));
            } else {
                state.mmio.insert(offset, value);
            }
            Ok(())
        }
    }

    impl DklIo for Model {
        fn with_dkl_lock<T>(
            &self,
            operation: impl FnOnce() -> Result<T, Error>,
        ) -> Result<T, Error> {
            let _guard = self.dkl_lock.lock().unwrap();
            operation()
        }
    }

    impl TcIo for Model {
        fn display_core_powered(&self) -> bool {
            true
        }
        fn tc_port_powered(&self, _port: TcPort) -> bool {
            true
        }
        fn tc_cold_blocked(&self, _port: TcPort) -> bool {
            true
        }
    }

    fn expected_rmw(old: u32, clear: u32, set: u32) -> u32 {
        (old & !clear) | set
    }

    #[test]
    fn source_tc_mode_hpd_and_lane_count_helpers_match_i915() {
        assert_eq!(tc_port_mode_name(TcPortMode::TbtAlt), "tbt-alt");
        assert_eq!(
            tc_phy_load_fia_params(3, true),
            FiaParams {
                fia_index: 1,
                port_index: 1
            }
        );
        assert_eq!(
            tc_phy_load_fia_params(3, false),
            FiaParams {
                fia_index: 0,
                port_index: 3
            }
        );
        assert!(tc_port_fixup_legacy_flag(true, 1 << TcPortMode::DpAlt as u8) == false);
        assert!(tc_port_fixup_legacy_flag(false, 1 << TcPortMode::Legacy as u8) == true);
        assert!(!tc_port_fixup_legacy_flag(false, 0));
        assert_eq!(
            tc_port_power_domain(TcPort::Tc4),
            PowerDomain::PortDdiLanesTc4
        );
        assert!(intel_tc_cold_requires_aux_pw(
            PowerDomain::AuxUsbc1,
            PowerDomain::AuxUsbc1
        ));
        assert!(!intel_tc_cold_requires_aux_pw(
            PowerDomain::TcColdOff,
            PowerDomain::AuxUsbc1
        ));
        assert_eq!(pin_assignment_name(TcPinAssignment::E), 'E');
        assert_eq!(decode_pin_assignment(4), TcPinAssignment::D);
        assert_eq!(
            get_pin_assignment(TcPortMode::TbtAlt, 5),
            TcPinAssignment::None
        );
        let config = read_pin_configuration(TcPortMode::DpAlt, 13, 0xc, 4);
        assert_eq!(
            config,
            TcPortConfiguration {
                pin_assignment: TcPinAssignment::D,
                max_lane_count: 2
            }
        );
        assert_eq!(
            intel_tc_port_get_pin_assignment(false, TcPinAssignment::C),
            TcPinAssignment::None
        );
        assert!(intel_tc_port_in_mode(
            true,
            TcPortMode::DpAlt,
            TcPortMode::DpAlt
        ));
        assert!(!intel_tc_port_in_mode(
            false,
            TcPortMode::DpAlt,
            TcPortMode::DpAlt
        ));
        assert!(intel_tc_port_in_tbt_alt_mode(true, TcPortMode::TbtAlt));
        assert!(intel_tc_port_in_dp_alt_mode(true, TcPortMode::DpAlt));
        assert!(intel_tc_port_in_legacy_mode(true, TcPortMode::Legacy));
        assert!(intel_tc_port_handles_hpd_glitches(true, false));
        assert!(!intel_tc_port_handles_hpd_glitches(true, true));
        for (mask, lanes) in [(1, 1), (2, 1), (4, 1), (8, 1), (3, 2), (12, 2), (15, 4)] {
            assert_eq!(icl_get_max_lane_count(mask), lanes);
        }
        assert_eq!(mtl_get_max_lane_count(TcPinAssignment::C), 4);
        assert_eq!(mtl_get_max_lane_count(TcPinAssignment::D), 2);
        assert_eq!(mtl_get_max_lane_count(TcPinAssignment::None), 0);
        assert_eq!(
            get_max_lane_count(TcPortMode::Legacy, 13, 1, TcPinAssignment::D),
            4
        );
        assert_eq!(intel_tc_port_max_lane_count(false, 1), 4);
    }

    #[test]
    fn adlp_hdmi_vbt_level_five_uses_both_lanes_loadgen_and_preserves_selector() {
        for port in [TcPort::Tc1, TcPort::Tc2] {
            let hip = 0x1735_9248;
            let model = Model::new(port, hip);
            let initial0 = model.value(port, 0x2c0);
            let initial1 = model.value(port, 0x2c4);
            let initial2 = model.value(port, 0x2c8);
            let lane1_initial2 = model.value(port, 0x12c8);

            adlp_tc_dkl_hdmi_set_signal_levels(&model, port, 148_500, 5, Wa16011342517::Active)
                .unwrap();

            let signal_mask = DKL_TX_PRESHOOT_MASK | DKL_TX_DE_EMPHASIS_MASK | DKL_TX_VSWING_MASK;
            let level_five = 5 << 8; // table index 5: vswing 0, deemphasis 5.
            assert_eq!(
                model.value(port, 0x2c0),
                expected_rmw(initial0, signal_mask, level_five)
            );
            assert_eq!(
                model.value(port, 0x2c4),
                expected_rmw(initial1, signal_mask, level_five)
            );
            assert_eq!(model.value(port, 0x0d00), 0);
            assert_eq!(model.value(port, 0x1000 + 0x0d00), 0);

            let loadgen = DKL_TX_DPCNTL2_LOADGEN_TX1_MASK | DKL_TX_DPCNTL2_LOADGEN_TX2_MASK;
            let lane0 = expected_rmw(
                expected_rmw(
                    expected_rmw(initial2, DKL_TX_DPCNTL2_WA_16011342517, 0),
                    DKL_TX_DPCNTL2_DP20BITMODE,
                    0,
                ),
                loadgen,
                2 << 5,
            );
            let lane1 = expected_rmw(
                expected_rmw(
                    expected_rmw(lane1_initial2, DKL_TX_DPCNTL2_WA_16011342517, 0),
                    DKL_TX_DPCNTL2_DP20BITMODE,
                    0,
                ),
                loadgen,
                (3 << 3) | (3 << 5),
            );
            assert_eq!(model.value(port, 0x2c8), lane0);
            assert_eq!(model.value(port, 0x12c8), lane1);
            assert_eq!(model.selector(), hip);
            assert_eq!(model.dkl_writes().len(), 12);
        }
    }

    #[test]
    fn wa_16011342517_clock_branch_matches_linux_literal_set_value() {
        let port = TcPort::Tc1;
        let model = Model::new(port, 0x1234_5678);
        let old = model.value(port, 0x2c8);
        adlp_tc_dkl_hdmi_set_signal_levels(&model, port, 594_000, 5, Wa16011342517::Active)
            .unwrap();
        // Linux 7.2.3 passes literal `1` as the RMW set value after clearing
        // bit 12; do not reinterpret it as `LOADGEN_SHARING_PMD_DISABLE`.
        let after_wa = expected_rmw(old, DKL_TX_DPCNTL2_WA_16011342517, 1);
        let after_dp = expected_rmw(after_wa, DKL_TX_DPCNTL2_DP20BITMODE, 0);
        let expected = expected_rmw(
            after_dp,
            DKL_TX_DPCNTL2_LOADGEN_TX1_MASK | DKL_TX_DPCNTL2_LOADGEN_TX2_MASK,
            2 << 5,
        );
        assert_eq!(model.value(port, 0x2c8), expected);
    }

    #[test]
    fn expected_phy_state_updates_only_source_signal_words() {
        let before = DklPhyState {
            uc_dw27: 0xfeed_beef,
            lanes: [
                DklLaneState {
                    pcs_dw5: 0x11,
                    dp_mode: 0x12,
                    lane_suspend: 0x13,
                    tx_control: [0xaaaa_aaaa, 0xbbbb_bbbb, 0x8123_4567],
                    fw_calibration: 0x14,
                    tx_dw17: 0x15,
                    tx_dw18: 0x16,
                },
                DklLaneState {
                    pcs_dw5: 0x21,
                    dp_mode: 0x22,
                    lane_suspend: 0x23,
                    tx_control: [0xcccc_cccc, 0xdddd_dddd, 0x1234_5678],
                    fw_calibration: 0x24,
                    tx_dw17: 0x25,
                    tx_dw18: 0x26,
                },
            ],
        };
        let expected = adlp_tc_dkl_hdmi_expected_phy_state(
            before,
            TcPort::Tc1,
            148_500,
            5,
            Wa16011342517::Active,
        )
        .unwrap();
        assert_eq!(expected.uc_dw27, before.uc_dw27);
        for lane in 0..2 {
            assert_eq!(expected.lanes[lane].pcs_dw5, before.lanes[lane].pcs_dw5);
            assert_eq!(expected.lanes[lane].dp_mode, before.lanes[lane].dp_mode);
            assert_eq!(expected.lanes[lane].lane_suspend, 0);
            assert_eq!(
                expected.lanes[lane].tx_control[0],
                5 << 8
                    | (before.lanes[lane].tx_control[0]
                        & !(DKL_TX_PRESHOOT_MASK | DKL_TX_DE_EMPHASIS_MASK | DKL_TX_VSWING_MASK))
            );
            assert_eq!(
                expected.lanes[lane].tx_control[1],
                5 << 8
                    | (before.lanes[lane].tx_control[1]
                        & !(DKL_TX_PRESHOOT_MASK | DKL_TX_DE_EMPHASIS_MASK | DKL_TX_VSWING_MASK))
            );
            assert_eq!(
                expected.lanes[lane].fw_calibration,
                before.lanes[lane].fw_calibration
            );
            assert_eq!(expected.lanes[lane].tx_dw17, before.lanes[lane].tx_dw17);
            assert_eq!(expected.lanes[lane].tx_dw18, before.lanes[lane].tx_dw18);
        }
        assert_eq!(expected.lanes[0].tx_control[2] & 0x107c, 0x40);
        assert_eq!(expected.lanes[1].tx_control[2] & 0x107c, 0x78);
        assert_eq!(
            expected.lanes[0].tx_control[2] & (DKL_TX_DPCNTL2_WA_16011342517 | 1),
            1
        );
        assert_eq!(
            expected.lanes[1].tx_control[2] & (DKL_TX_DPCNTL2_WA_16011342517 | 1),
            0
        );
    }

    #[test]
    fn unknown_wa_and_unavailable_preset_refuse_before_any_phy_write() {
        let model = Model::new(TcPort::Tc1, 0x1234_5678);
        assert_eq!(
            adlp_tc_dkl_hdmi_set_signal_levels(
                &model,
                TcPort::Tc1,
                148_500,
                5,
                Wa16011342517::Unknown,
            ),
            Err(Error::Refused)
        );
        assert_eq!(
            adlp_tc_dkl_hdmi_set_signal_levels(
                &model,
                TcPort::Tc1,
                148_500,
                10,
                Wa16011342517::Active,
            ),
            Err(Error::Refused)
        );
        assert!(model.dkl_writes().is_empty());
    }

    #[test]
    fn failed_phy_prefix_restores_shared_hip_selector_for_outer_rollback() {
        let port = TcPort::Tc2;
        let hip = 0x7654_3210;
        let model = Model::new(port, hip);
        let failed_aperture = 0x168000 + port.index() * 0x1000 + 0x2c4;
        model.set_fail_write_once(failed_aperture);
        assert_eq!(
            adlp_tc_dkl_hdmi_set_signal_levels(&model, port, 148_500, 5, Wa16011342517::Active,),
            Err(Error::Unavailable(failed_aperture))
        );
        assert_eq!(model.selector(), hip);
        // This helper never pretends to roll back already-landed PHY writes;
        // the enclosing modeset transaction owns the captured before-image.
        assert!(!model.dkl_writes().is_empty());
    }
}
