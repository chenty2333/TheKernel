// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_tc.c (display-12/13 mode and
// ownership paths). The state machine is kept separate from tc.rs so its
// framework adapters can be reviewed without changing the existing MMIO port.
//
// This is a portable mechanism translation: `TcStateIo` supplies i915's power,
// lock/work, and PHY-register accessors. The callbacks are deliberately narrow
// and preserve the source's acquisition/release ordering and mode fallbacks.

use crate::{
    Error,
    dkl_phy::TcPort,
    power_map::PowerDomain,
    tc::{
        TcIo, TcPhyFamily, TcPinAssignment, TcPortConfiguration, TcPortMode,
        adlp_tc_phy_is_owned as tc_adlp_tc_phy_is_owned,
        adlp_tc_phy_is_ready as tc_adlp_tc_phy_is_ready, buffer_register, pin_assignment_name,
        tc_phy_cold_off_domain as tc_family_cold_off_domain, tc_phy_load_fia_params,
        tc_port_fixup_legacy_flag, tc_port_power_domain,
    },
};

const MODE_BIT_LEGACY: u32 = 1 << (TcPortMode::Legacy as u8);
const MODE_BIT_TBT_ALT: u32 = 1 << (TcPortMode::TbtAlt as u8);
const MODE_BIT_DP_ALT: u32 = 1 << (TcPortMode::DpAlt as u8);
const PHY_READY_POLL_INTERVAL_US: u32 = 1_000;
const PHY_READY_POLL_TIMEOUT_US: u32 = 500_000;

/// Persistent subset of `struct intel_tc_port` needed by display-12/13 paths.
/// `lock_wakeref` models the source's ref-tracker presence, while `lock_domain`
/// preserves the selected power domain so its matching put is observable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TcPortState {
    pub port: TcPort,
    pub family: TcPhyFamily,
    pub legacy_port: bool,
    pub port_name: &'static str,
    pub mode: TcPortMode,
    pub init_mode: TcPortMode,
    pub pin_assignment: TcPinAssignment,
    pub max_lane_count: u8,
    pub phy_fia: u8,
    pub phy_fia_index: u8,
    pub link_refcount: i32,
    pub lock_wakeref: bool,
    pub lock_domain: Option<PowerDomain>,
    /// Legacy AUX domain resolved by the enclosing display-power manager.
    pub legacy_aux_domain: PowerDomain,
}

impl TcPortState {
    pub const fn new(
        port: TcPort,
        family: TcPhyFamily,
        legacy_port: bool,
        port_name: &'static str,
        legacy_aux_domain: PowerDomain,
    ) -> Self {
        Self {
            port,
            family,
            legacy_port,
            port_name,
            mode: TcPortMode::Disconnected,
            init_mode: TcPortMode::Disconnected,
            pin_assignment: TcPinAssignment::None,
            max_lane_count: 0,
            phy_fia: 0,
            phy_fia_index: 0,
            link_refcount: 0,
            lock_wakeref: false,
            lock_domain: None,
            legacy_aux_domain,
        }
    }

    pub const fn cold_off_domain(&self) -> PowerDomain {
        tc_phy_cold_off_domain(self)
    }
}

/// Source-order hardware/work/power hooks supplied by the enclosing display
/// driver. `get_*`/`put_*` model `intel_display_power_get/put`; lock methods
/// model the port mutex; delayed-work hooks preserve when source work is
/// cancelled, queued, synchronously flushed, or drained during suspend.
///
/// PHY register callbacks must implement the selected family operation with
/// the register ordering and powered-domain preconditions of its i915 handler.
pub trait TcStateIo: TcIo {
    fn lock_port(&mut self, port: TcPort) -> Result<(), Error>;
    fn unlock_port(&mut self, port: TcPort);
    fn get_power(&mut self, domain: PowerDomain) -> Result<(), Error>;
    fn put_power(&mut self, domain: PowerDomain) -> Result<(), Error>;
    fn port_enabled(&mut self, port: TcPort) -> Result<bool, Error>;
    fn aux_powered(&self, domain: PowerDomain) -> bool;
    fn power_flush_work(&mut self);

    /// Read ICL/TGL FIA + PCH ISR under the current cold-off domain, or ADL-P
    /// CPU + PCH ISR under DISPLAY_CORE; the snapshot is decoded below.
    fn read_hpd_snapshot(
        &mut self,
        family: TcPhyFamily,
        state: &TcPortState,
    ) -> Result<TcHpdSnapshot, Error>;
    /// Read FIA pin assignment and lane topology, applying tc.rs's
    /// `get_pin_assignment()` / `get_max_lane_count()` decisions unchanged.
    fn read_pin_configuration(&mut self, state: &TcPortState)
    -> Result<TcPortConfiguration, Error>;
    fn tgl_modular_fia_status(&mut self, state: &TcPortState) -> Result<u32, Error>;
    fn wait_for_phy_ready(
        &mut self,
        state: &TcPortState,
        interval_us: u32,
        timeout_us: u32,
    ) -> Result<bool, Error>;
    /// Access one exact FIA register selected by state. Callers enforce the
    /// source's display-core and TC-cold power prerequisites in source order.
    fn read_icl_fia_reg(
        &mut self,
        state: &TcPortState,
        register: IclTcRegister,
    ) -> Result<u32, Error>;
    fn write_icl_fia_reg(
        &mut self,
        state: &TcPortState,
        register: IclTcRegister,
        value: u32,
    ) -> Result<(), Error>;

    fn stream_state(&mut self, state: &TcPortState) -> Result<TcStreamState, Error>;

    fn diagnostic(&mut self, diagnostic: TcDiagnostic);
    fn cancel_disconnect_work(&mut self, port: TcPort);
    fn queue_disconnect_work(&mut self, port: TcPort, delay_ms: u32);
    fn flush_disconnect_work(&mut self, port: TcPort);
    fn cancel_link_reset_work(&mut self, port: TcPort, synchronous: bool);
    fn port_lock_is_held(&self, port: TcPort) -> bool;
    fn queue_link_reset_work(&mut self, port: TcPort, delay_ms: u32);
    fn modeset_mutex_lock(&mut self, port: TcPort) -> Result<(), Error>;
    fn modeset_mutex_unlock(&mut self, port: TcPort);
    /// Allocate an internal atomic state and lock context, run `operation` via
    /// the source lock-context retry loop, then release both on every exit.
    fn with_atomic_modeset_retry<T>(
        &mut self,
        port: TcPort,
        operation: impl FnMut(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error>
    where
        Self: Sized;
    /// drm_modeset_lock(...connection_mutex, ctx); acquired lock is owned by
    /// the active modeset context and is released by the retry wrapper above.
    fn connection_mutex_lock(&mut self, port: TcPort) -> Result<(), Error>;
    fn active_pipe_mask(&mut self, port: TcPort) -> Result<u8, Error>;
    fn mark_connectors_changed(&mut self, port: TcPort, pipe_mask: u8) -> Result<(), Error>;
    fn atomic_commit(&mut self, port: TcPort) -> Result<(), Error>;
}

/// Diagnostics that correspond to source `drm_WARN[_ONCE]`, `drm_dbg_kms`,
/// and `drm_err` sites. They are hooks only; they never replace a transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcDiagnostic {
    UnexpectedLiveModes(u32),
    PhyOwnedButNotReady,
    PhyModeMismatch,
    LegacyLaneCount(u8),
    NonDpAltMode,
    SuddenDisconnect,
    TooFewLanes {
        maximum: u8,
        required: u8,
    },
    OwnershipUnavailable {
        ready: bool,
    },
    PhyNotReady,
    UnexpectedAuxPower,
    EnabledButDisconnectedLegacy,
    ActiveStreamsOnDisconnectedPhy(u32),
    DisabledPortLeftInMode(TcPortMode),
    LinkResetFailed,
    ConnectFailed,
    PowerDomainChanged {
        held: PowerDomain,
        current: PowerDomain,
    },
    TglFiaUnavailable,
    FiaUnavailable,
    TcColdNotBlocked,
    UnexpectedInitialState,
    ModeReset {
        old: TcPortMode,
        new: TcPortMode,
        pin: char,
        max_lanes: u8,
    },
}

/// i915's DPLL class distinction used by `tc_phy_is_connected()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcPllType {
    TbtDefault,
    MgPhy,
}

/// i915's two active-stream sources: MST count, or one enabled CRTC with its
/// resolved DDI PLL class. `Disabled` corresponds to a null/inactive CRTC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcStreamState {
    Mst(u32),
    Crtc { active: bool, pll: TcPllType },
    Disabled,
}

/// Register snapshots needed by the two HPD paths shared by display-12/13.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcHpdSnapshot {
    Icl {
        fia_isr: u32,
        pch_isr: u32,
        pch_hpd_bit: u32,
        fia_index: u8,
    },
    Adlp {
        cpu_isr: u32,
        pch_isr: u32,
        cpu_hpd_bits: u32,
        pch_hpd_bit: u32,
    },
}

/// FIA registers addressed by the ICL/TGL ownership callbacks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IclTcRegister {
    DflexDpsp,
    DflexDppms,
    DflexDpcsss,
}

/// Acquire the family-selected TC-cold blocking domain and report which one.
// upstream: intel_tc.c __tc_cold_block()
fn __tc_cold_block(io: &mut impl TcStateIo, state: &TcPortState) -> Result<PowerDomain, Error> {
    let domain = state.cold_off_domain();
    io.get_power(domain)?;
    Ok(domain)
}

/// Retain the cold-block reference that protects the active mode.
// upstream: intel_tc.c tc_cold_block()
fn tc_cold_block(io: &mut impl TcStateIo, state: &mut TcPortState) -> Result<(), Error> {
    let domain = __tc_cold_block(io, state)?;
    state.lock_wakeref = true;
    state.lock_domain = Some(domain);
    Ok(())
}

/// Drop the power reference for the supplied family-selected domain.
// upstream: intel_tc.c __tc_cold_unblock()
fn __tc_cold_unblock(io: &mut impl TcStateIo, domain: PowerDomain) -> Result<(), Error> {
    io.put_power(domain)
}

/// Consume the stored mode reference and release the current mode's domain.
// upstream: intel_tc.c tc_cold_unblock()
fn tc_cold_unblock(io: &mut impl TcStateIo, state: &mut TcPortState) -> Result<(), Error> {
    let held_domain = state.lock_domain.take().ok_or(Error::Refused)?;
    let current_domain = state.cold_off_domain();
    if !state.lock_wakeref {
        return Err(Error::Refused);
    }
    state.lock_wakeref = false;
    if held_domain != current_domain {
        io.diagnostic(TcDiagnostic::PowerDomainChanged {
            held: held_domain,
            current: current_domain,
        });
    }
    __tc_cold_unblock(io, current_domain)
}

/// Resolve the cold-off domain through the selected PHY callback.
// upstream: intel_tc.c tc_phy_cold_off_domain()
pub const fn tc_phy_cold_off_domain(state: &TcPortState) -> PowerDomain {
    tc_family_cold_off_domain(
        state.family,
        state.mode,
        state.legacy_port,
        state.legacy_aux_domain,
    )
}

/// Guard ADL-P TCSS status reads with DISPLAY_CORE power.
// upstream: intel_tc.c assert_display_core_power_enabled()
pub fn assert_display_core_power_enabled(io: &impl TcStateIo) -> Result<(), Error> {
    if io.display_core_powered() {
        Ok(())
    } else {
        Err(Error::Refused)
    }
}

/// Guard ICL/TGL FIA status accesses with the selected TC-cold domain held.
// upstream: intel_tc.c assert_tc_cold_blocked()
pub fn assert_tc_cold_blocked(io: &impl TcStateIo, port: TcPort) -> Result<(), Error> {
    if io.tc_cold_blocked(port) {
        Ok(())
    } else {
        Err(Error::Refused)
    }
}

/// Guard ADL-P DDI_BUF_CTL accesses with the TC port lane domain powered.
// upstream: intel_tc.c assert_tc_port_power_enabled()
pub fn assert_tc_port_power_enabled(io: &impl TcStateIo, port: TcPort) -> Result<(), Error> {
    if io.tc_port_powered(port) {
        Ok(())
    } else {
        Err(Error::Refused)
    }
}

/// `tc_phy_hpd_live_status()` checks the one-mode invariant after dispatch.
// upstream: intel_tc.c tc_phy_hpd_live_status()
pub fn tc_phy_hpd_live_status(io: &mut impl TcStateIo, state: &TcPortState) -> Result<u32, Error> {
    let snapshot = io.read_hpd_snapshot(state.family, state)?;
    let status = match snapshot {
        TcHpdSnapshot::Icl {
            fia_isr,
            pch_isr,
            pch_hpd_bit,
            fia_index,
        } => icl_tc_phy_hpd_live_status(fia_isr, pch_isr, pch_hpd_bit, fia_index),
        TcHpdSnapshot::Adlp {
            cpu_isr,
            pch_isr,
            cpu_hpd_bits,
            pch_hpd_bit,
        } => adlp_tc_phy_hpd_live_status(cpu_isr, pch_isr, cpu_hpd_bits, pch_hpd_bit),
    };
    if status.count_ones() > 1 {
        io.diagnostic(TcDiagnostic::UnexpectedLiveModes(status));
    }
    Ok(status)
}

/// ICL's FIA/PCH HPD sources; TGL reuses this exact handler.
// upstream: intel_tc.c icl_tc_phy_hpd_live_status()
pub const fn icl_tc_phy_hpd_live_status(
    fia_isr: u32,
    pch_isr: u32,
    pch_hpd_bit: u32,
    fia_index: u8,
) -> u32 {
    if fia_isr == u32::MAX {
        return 0;
    }
    let mut mask = 0;
    if fia_isr & (1 << (fia_index as u32 * 8 + 6)) != 0 {
        mask |= MODE_BIT_TBT_ALT;
    }
    if fia_isr & (1 << (fia_index as u32 * 8 + 5)) != 0 {
        mask |= MODE_BIT_DP_ALT;
    }
    if pch_isr & pch_hpd_bit != 0 {
        mask |= MODE_BIT_LEGACY;
    }
    mask
}

/// ADL-P's CPU TC/TBT and PCH legacy HPD sources.
// upstream: intel_tc.c adlp_tc_phy_hpd_live_status()
pub const fn adlp_tc_phy_hpd_live_status(
    cpu_isr: u32,
    pch_isr: u32,
    cpu_hpd_bits: u32,
    pch_hpd_bit: u32,
) -> u32 {
    let tc_bits = cpu_hpd_bits & (0x3f << 16);
    let tbt_bits = cpu_hpd_bits & 0x3f;
    let mut mask = 0;
    if cpu_isr & tc_bits != 0 {
        mask |= MODE_BIT_DP_ALT;
    }
    if cpu_isr & tbt_bits != 0 {
        mask |= MODE_BIT_TBT_ALT;
    }
    if pch_isr & pch_hpd_bit != 0 {
        mask |= MODE_BIT_LEGACY;
    }
    mask
}

/// `tc_phy_is_ready()` dispatches to the family's PHY status implementation.
// upstream: intel_tc.c tc_phy_is_ready()
pub fn tc_phy_is_ready(io: &mut impl TcStateIo, state: &TcPortState) -> Result<bool, Error> {
    match state.family {
        TcPhyFamily::Icl | TcPhyFamily::TigerLake => icl_tc_phy_is_ready(io, state),
        TcPhyFamily::AlderLakeP => adlp_tc_phy_is_ready(io, state),
    }
}

/// `tc_phy_is_owned()` dispatches to the family's ownership implementation.
// upstream: intel_tc.c tc_phy_is_owned()
pub fn tc_phy_is_owned(io: &mut impl TcStateIo, state: &TcPortState) -> Result<bool, Error> {
    match state.family {
        TcPhyFamily::Icl | TcPhyFamily::TigerLake => icl_tc_phy_is_owned(io, state),
        TcPhyFamily::AlderLakeP => adlp_tc_phy_is_owned(io, state),
    }
}

/// ICL/TGL status-complete flag; TGL shares the ICL register protocol.
// upstream: intel_tc.c icl_tc_phy_is_ready()
pub fn icl_tc_phy_is_ready(io: &mut impl TcStateIo, state: &TcPortState) -> Result<bool, Error> {
    assert_tc_cold_blocked(io, state.port)?;
    let value = io.read_icl_fia_reg(state, IclTcRegister::DflexDppms)?;
    if value == u32::MAX {
        return Ok(false);
    }
    Ok(value & (1 << state.phy_fia_index) != 0)
}

/// ICL/TGL ownership request: read, update only this FIA port's bit, then write.
// upstream: intel_tc.c icl_tc_phy_take_ownership()
pub fn icl_tc_phy_take_ownership(
    io: &mut impl TcStateIo,
    state: &TcPortState,
    take: bool,
) -> Result<bool, Error> {
    assert_tc_cold_blocked(io, state.port)?;
    let mut value = io.read_icl_fia_reg(state, IclTcRegister::DflexDpcsss)?;
    if value == u32::MAX {
        return Ok(false);
    }
    let mask = 1 << state.phy_fia_index;
    value &= !mask;
    if take {
        value |= mask;
    }
    io.write_icl_fia_reg(state, IclTcRegister::DflexDpcsss, value)?;
    Ok(true)
}

/// ICL/TGL ownership status, false when the FIA is in TCCOLD.
// upstream: intel_tc.c icl_tc_phy_is_owned()
pub fn icl_tc_phy_is_owned(io: &mut impl TcStateIo, state: &TcPortState) -> Result<bool, Error> {
    assert_tc_cold_blocked(io, state.port)?;
    let value = io.read_icl_fia_reg(state, IclTcRegister::DflexDpcsss)?;
    if value == u32::MAX {
        return Ok(false);
    }
    Ok(value & (1 << state.phy_fia_index) != 0)
}

/// ADL-P sets the ownership bit in DDI_BUF_CTL with intel_de_rmw semantics.
// upstream: intel_tc.c adlp_tc_phy_take_ownership()
pub fn adlp_tc_phy_take_ownership(
    io: &mut impl TcStateIo,
    state: &TcPortState,
    take: bool,
) -> Result<bool, Error> {
    assert_tc_port_power_enabled(io, state.port)?;
    const TC_PHY_OWNERSHIP: u32 = 1 << 6;
    let register = buffer_register(state.port);
    let old = io.read32(register)?;
    let new = (old & !TC_PHY_OWNERSHIP) | if take { TC_PHY_OWNERSHIP } else { 0 };
    io.write32(register, new)?;
    Ok(true)
}

/// ADL-P get-ready wrapper retains DISPLAY_CORE as its required powered domain.
// upstream: intel_tc.c adlp_tc_phy_is_ready()
pub fn adlp_tc_phy_is_ready(io: &mut impl TcStateIo, state: &TcPortState) -> Result<bool, Error> {
    assert_display_core_power_enabled(io)?;
    tc_adlp_tc_phy_is_ready(io, state.port)
}

/// ADL-P get-owned wrapper retains PORT_DDI_LANES_TC(n) as its required domain.
// upstream: intel_tc.c adlp_tc_phy_is_owned()
pub fn adlp_tc_phy_is_owned(io: &mut impl TcStateIo, state: &TcPortState) -> Result<bool, Error> {
    assert_tc_port_power_enabled(io, state.port)?;
    tc_adlp_tc_phy_is_owned(io, state.port)
}

/// Platform ownership mutation dispatch used by the connect/disconnect flows.
pub fn take_phy_ownership(
    io: &mut impl TcStateIo,
    state: &TcPortState,
    take: bool,
) -> Result<bool, Error> {
    match state.family {
        TcPhyFamily::Icl | TcPhyFamily::TigerLake => icl_tc_phy_take_ownership(io, state, take),
        TcPhyFamily::AlderLakeP => adlp_tc_phy_take_ownership(io, state, take),
    }
}

/// i915 treats PHYs as display-owned only when both status bits are asserted on
/// display-12/13 (the display-20 ownership-only exception is out of scope).
// upstream: intel_tc.c tc_phy_owned_by_display()
pub fn tc_phy_owned_by_display(io: &mut impl TcStateIo, ready: bool, owned: bool) -> bool {
    if owned && !ready {
        io.diagnostic(TcDiagnostic::PhyOwnedButNotReady);
    }
    ready && owned
}

/// Wait for IOM PHY readiness using the source's 1 ms interval and 500 ms cap.
// upstream: intel_tc.c tc_phy_wait_for_ready()
pub fn tc_phy_wait_for_ready(io: &mut impl TcStateIo, state: &TcPortState) -> Result<bool, Error> {
    io.wait_for_phy_ready(state, PHY_READY_POLL_INTERVAL_US, PHY_READY_POLL_TIMEOUT_US)
}

/// Convert the single set live-status bit to the corresponding TC mode.
// upstream: intel_tc.c hpd_mask_to_tc_mode()
pub const fn hpd_mask_to_tc_mode(live_status_mask: u32) -> TcPortMode {
    if live_status_mask == 0 {
        TcPortMode::Disconnected
    } else {
        match 31 - live_status_mask.leading_zeros() {
            0 => TcPortMode::Disconnected,
            1 => TcPortMode::TbtAlt,
            2 => TcPortMode::DpAlt,
            3 => TcPortMode::Legacy,
            _ => TcPortMode::Disconnected,
        }
    }
}

/// Convert the HPD status observed through the platform PHY operation.
// upstream: intel_tc.c tc_phy_hpd_live_mode()
pub fn tc_phy_hpd_live_mode(
    io: &mut impl TcStateIo,
    state: &TcPortState,
) -> Result<TcPortMode, Error> {
    Ok(hpd_mask_to_tc_mode(tc_phy_hpd_live_status(io, state)?))
}

/// Mode to use when the PHY is owned by display.
// upstream: intel_tc.c get_tc_mode_in_phy_owned_state()
pub fn get_tc_mode_in_phy_owned_state(state: &TcPortState, live_mode: TcPortMode) -> TcPortMode {
    match live_mode {
        TcPortMode::Legacy | TcPortMode::DpAlt => live_mode,
        TcPortMode::TbtAlt | TcPortMode::Disconnected => {
            if state.legacy_port {
                TcPortMode::Legacy
            } else {
                TcPortMode::DpAlt
            }
        }
    }
}

/// Mode to use when the PHY is not owned by display.
// upstream: intel_tc.c get_tc_mode_in_phy_not_owned_state()
pub fn get_tc_mode_in_phy_not_owned_state(
    state: &TcPortState,
    live_mode: TcPortMode,
) -> TcPortMode {
    match live_mode {
        TcPortMode::Legacy => TcPortMode::Disconnected,
        TcPortMode::DpAlt | TcPortMode::TbtAlt => TcPortMode::TbtAlt,
        TcPortMode::Disconnected => {
            if state.legacy_port {
                TcPortMode::Disconnected
            } else {
                TcPortMode::TbtAlt
            }
        }
    }
}

/// Read the live, ready, and ownership states in the same order as i915 and
/// infer a stable mode. Legacy ports wait for firmware initialization first.
// upstream: intel_tc.c tc_phy_get_current_mode()
pub fn tc_phy_get_current_mode(
    io: &mut impl TcStateIo,
    state: &TcPortState,
) -> Result<TcPortMode, Error> {
    let live_mode = tc_phy_hpd_live_mode(io, state)?;
    if state.legacy_port {
        let _ = tc_phy_wait_for_ready(io, state)?;
    }
    let ready = tc_phy_is_ready(io, state)?;
    let owned = tc_phy_is_owned(io, state)?;
    let mode = if !tc_phy_owned_by_display(io, ready, owned) {
        get_tc_mode_in_phy_not_owned_state(state, live_mode)
    } else {
        if live_mode == TcPortMode::TbtAlt {
            io.diagnostic(TcDiagnostic::PhyModeMismatch);
        }
        get_tc_mode_in_phy_owned_state(state, live_mode)
    };
    Ok(mode)
}

/// Default mode is legacy on hardwired legacy ports and TBT-alt otherwise.
// upstream: intel_tc.c default_tc_mode()
pub const fn default_tc_mode(state: &TcPortState) -> TcPortMode {
    if state.legacy_port {
        TcPortMode::Legacy
    } else {
        TcPortMode::TbtAlt
    }
}

/// Map live status to its mode, falling back to the port's default when empty.
// upstream: intel_tc.c hpd_mask_to_target_mode()
pub const fn hpd_mask_to_target_mode(state: &TcPortState, live_status_mask: u32) -> TcPortMode {
    let mode = hpd_mask_to_tc_mode(live_status_mask);
    if matches!(mode, TcPortMode::Disconnected) {
        default_tc_mode(state)
    } else {
        mode
    }
}

/// Query target mode from the live HPD bitmap.
// upstream: intel_tc.c tc_phy_get_target_mode()
pub fn tc_phy_get_target_mode(
    io: &mut impl TcStateIo,
    state: &TcPortState,
) -> Result<TcPortMode, Error> {
    Ok(hpd_mask_to_target_mode(
        state,
        tc_phy_hpd_live_status(io, state)?,
    ))
}

/// Store the current pin assignment and lane count after the FIA policy read.
// upstream: intel_tc.c read_pin_configuration()
pub fn read_pin_configuration(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<(), Error> {
    let configuration = io.read_pin_configuration(state)?;
    state.pin_assignment = configuration.pin_assignment;
    state.max_lane_count = configuration.max_lane_count;
    Ok(())
}

/// Read the FIA lane-assignment field under DISPLAY_CORE, then validate the
/// source's TC-cold precondition and extract this port's nibble.
// upstream: intel_tc.c get_lane_mask()
pub fn get_lane_mask(io: &mut impl TcStateIo, state: &TcPortState) -> Result<u8, Error> {
    io.get_power(PowerDomain::DisplayCore)?;
    let value = io.read_icl_fia_reg(state, IclTcRegister::DflexDpsp);
    let put_result = io.put_power(PowerDomain::DisplayCore);
    let value = value?;
    put_result?;
    if value == u32::MAX {
        io.diagnostic(TcDiagnostic::FiaUnavailable);
    }
    if !io.tc_cold_blocked(state.port) {
        io.diagnostic(TcDiagnostic::TcColdNotBlocked);
    }
    let shift = u32::from(state.phy_fia_index) * 8;
    Ok(((value >> shift) & 0xf) as u8)
}

/// Verify the handoff state after ownership: legacy is four-lane, DP-alt must
/// remain live and provide at least the requested lane count.
// upstream: intel_tc.c tc_phy_verify_legacy_or_dp_alt_mode()
pub fn tc_phy_verify_legacy_or_dp_alt_mode(
    io: &mut impl TcStateIo,
    state: &TcPortState,
    required_lanes: u8,
) -> Result<bool, Error> {
    let max_lanes = state.max_lane_count;
    if state.mode == TcPortMode::Legacy {
        if max_lanes != 4 {
            io.diagnostic(TcDiagnostic::LegacyLaneCount(max_lanes));
        }
        return Ok(true);
    }
    if state.mode != TcPortMode::DpAlt {
        io.diagnostic(TcDiagnostic::NonDpAltMode);
    }
    if tc_phy_hpd_live_status(io, state)? & MODE_BIT_DP_ALT == 0 {
        io.diagnostic(TcDiagnostic::SuddenDisconnect);
        return Ok(false);
    }
    if max_lanes < required_lanes {
        io.diagnostic(TcDiagnostic::TooFewLanes {
            maximum: max_lanes,
            required: required_lanes,
        });
        return Ok(false);
    }
    Ok(true)
}

/// ICL/TGL hardware-state readout reserves the cold-off domain while sampling;
/// an active mode retains a second reference in `lock_wakeref`.
// upstream: intel_tc.c icl_tc_phy_get_hw_state()
pub fn icl_tc_phy_get_hw_state(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<(), Error> {
    let sampling_domain = state.cold_off_domain();
    io.get_power(sampling_domain)?;
    let result: Result<(), Error> = (|| {
        state.mode = tc_phy_get_current_mode(io, state)?;
        if state.mode != TcPortMode::Disconnected {
            tc_cold_block(io, state)?;
            read_pin_configuration(io, state)?;
        }
        Ok(())
    })();
    let put_result = io.put_power(sampling_domain);
    result?;
    put_result
}

/// ADL-P keeps PORT_DDI_LANES_TC(n) live during status and ownership readout.
// upstream: intel_tc.c adlp_tc_phy_get_hw_state()
pub fn adlp_tc_phy_get_hw_state(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<(), Error> {
    let port_domain = tc_port_power_domain(state.port);
    io.get_power(port_domain)?;
    let result = (|| {
        state.mode = tc_phy_get_current_mode(io, state)?;
        if state.mode != TcPortMode::Disconnected {
            tc_cold_block(io, state)?;
            read_pin_configuration(io, state)?;
        }
        Ok(())
    })();
    let put_result = io.put_power(port_domain);
    result?;
    put_result
}

/// Shared ICL/TGL connect flow. TBT-alt takes a cold-off reference but does not
/// request display ownership; legacy and DP-alt perform FIA handoff and verify.
// upstream: intel_tc.c icl_tc_phy_connect()
pub fn icl_tc_phy_connect(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
    required_lanes: u8,
) -> Result<bool, Error> {
    tc_cold_block(io, state)?;
    if state.mode == TcPortMode::TbtAlt {
        return read_pin_configuration(io, state).map(|()| true);
    }

    let ready = tc_phy_is_ready(io, state)?;
    let owned = if ready {
        take_phy_ownership(io, state, true)?
    } else {
        false
    };
    if (!ready || !owned) && state.mode != TcPortMode::Legacy {
        io.diagnostic(TcDiagnostic::OwnershipUnavailable { ready });
        tc_cold_unblock(io, state)?;
        return Ok(false);
    }

    if let Err(error) = read_pin_configuration(io, state) {
        let _ = take_phy_ownership(io, state, false);
        let _ = tc_cold_unblock(io, state);
        return Err(error);
    }
    match tc_phy_verify_legacy_or_dp_alt_mode(io, state, required_lanes) {
        Ok(true) => Ok(true),
        Ok(false) => {
            take_phy_ownership(io, state, false)?;
            tc_cold_unblock(io, state)?;
            Ok(false)
        }
        Err(error) => {
            let _ = take_phy_ownership(io, state, false);
            let _ = tc_cold_unblock(io, state);
            Err(error)
        }
    }
}

/// Disconnect flow releases FIA ownership for legacy/DP-alt before dropping
/// the retained cold-off reference; TBT-alt only drops that reference.
// upstream: intel_tc.c icl_tc_phy_disconnect()
pub fn icl_tc_phy_disconnect(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<(), Error> {
    match state.mode {
        TcPortMode::Legacy | TcPortMode::DpAlt => {
            let _ = take_phy_ownership(io, state, false)?;
            tc_cold_unblock(io, state)
        }
        TcPortMode::TbtAlt => tc_cold_unblock(io, state),
        TcPortMode::Disconnected => Err(Error::Refused),
    }
}

/// ADL-P connect holds lane power while changing ownership and readiness; its
/// TBT-alt path skips ownership and holds only the selected cold-off domain.
// upstream: intel_tc.c adlp_tc_phy_connect()
pub fn adlp_tc_phy_connect(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
    required_lanes: u8,
) -> Result<bool, Error> {
    if state.mode == TcPortMode::TbtAlt {
        tc_cold_block(io, state)?;
        return read_pin_configuration(io, state).map(|()| true);
    }

    let port_domain = tc_port_power_domain(state.port);
    io.get_power(port_domain)?;
    let attempt = (|| {
        let took_ownership = take_phy_ownership(io, state, true)?;
        if !took_ownership && state.mode != TcPortMode::Legacy {
            io.diagnostic(TcDiagnostic::OwnershipUnavailable { ready: false });
            return Ok(false);
        }
        let ready = tc_phy_is_ready(io, state)?;
        if !ready && state.mode != TcPortMode::Legacy {
            io.diagnostic(TcDiagnostic::PhyNotReady);
            let _ = take_phy_ownership(io, state, false)?;
            return Ok(false);
        }

        tc_cold_block(io, state)?;
        if let Err(error) = read_pin_configuration(io, state) {
            let _ = tc_cold_unblock(io, state);
            let _ = take_phy_ownership(io, state, false);
            return Err(error);
        }
        match tc_phy_verify_legacy_or_dp_alt_mode(io, state, required_lanes) {
            Ok(true) => Ok(true),
            Ok(false) => {
                tc_cold_unblock(io, state)?;
                let _ = take_phy_ownership(io, state, false)?;
                Ok(false)
            }
            Err(error) => {
                let _ = tc_cold_unblock(io, state);
                let _ = take_phy_ownership(io, state, false);
                Err(error)
            }
        }
    })();
    let put_result = io.put_power(port_domain);
    match attempt {
        Ok(connected) => {
            put_result?;
            Ok(connected)
        }
        Err(error) => {
            let _ = put_result;
            Err(error)
        }
    }
}

/// ADL-P disconnect drops cold-off power first, then relinquishes ownership for
/// display-owned modes while port-lane power remains held.
// upstream: intel_tc.c adlp_tc_phy_disconnect()
pub fn adlp_tc_phy_disconnect(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<(), Error> {
    let port_domain = tc_port_power_domain(state.port);
    io.get_power(port_domain)?;
    let cold_result = tc_cold_unblock(io, state);
    let ownership_result = match state.mode {
        TcPortMode::Legacy | TcPortMode::DpAlt => take_phy_ownership(io, state, false).map(|_| ()),
        TcPortMode::TbtAlt => Ok(()),
        TcPortMode::Disconnected => Err(Error::Refused),
    };
    let put_result = io.put_power(port_domain);
    cold_result?;
    ownership_result?;
    put_result
}

/// ADL-P hardwires the modular-FIA mapping; each FIA houses two TC ports.
// upstream: intel_tc.c adlp_tc_phy_init()
pub fn adlp_tc_phy_init(state: &mut TcPortState) {
    let params = tc_phy_load_fia_params(state.port.index() as u8, true);
    state.phy_fia = params.fia_index;
    state.phy_fia_index = params.port_index;
}

/// ICL's non-modular FIA port mapping (retained for the shared callback family).
// upstream: intel_tc.c icl_tc_phy_init()
pub fn icl_tc_phy_init(state: &mut TcPortState) {
    let params = tc_phy_load_fia_params(state.port.index() as u8, false);
    state.phy_fia = params.fia_index;
    state.phy_fia_index = params.port_index;
}

/// TGL reads MODULAR_FIA_MASK under the TC-cold power domain before applying
/// the source's shared/non-modular port mapping.
// upstream: intel_tc.c tgl_tc_phy_init()
pub fn tgl_tc_phy_init(io: &mut impl TcStateIo, state: &mut TcPortState) -> Result<(), Error> {
    let domain = state.cold_off_domain();
    io.get_power(domain)?;
    let status = io.tgl_modular_fia_status(state);
    let put_result = io.put_power(domain);
    let status = status?;
    put_result?;
    if status == u32::MAX {
        io.diagnostic(TcDiagnostic::TglFiaUnavailable);
    }
    let params = tc_phy_load_fia_params(state.port.index() as u8, status & (1 << 4) != 0);
    state.phy_fia = params.fia_index;
    state.phy_fia_index = params.port_index;
    Ok(())
}

/// Serialize platform PHY initialization under the Type-C port mutex.
// upstream: intel_tc.c tc_phy_init()
pub fn tc_phy_init(io: &mut impl TcStateIo, state: &mut TcPortState) -> Result<(), Error> {
    io.lock_port(state.port)?;
    let result = match state.family {
        TcPhyFamily::AlderLakeP => {
            adlp_tc_phy_init(state);
            Ok(())
        }
        TcPhyFamily::TigerLake => tgl_tc_phy_init(io, state),
        TcPhyFamily::Icl => {
            icl_tc_phy_init(state);
            Ok(())
        }
    };
    io.unlock_port(state.port);
    result
}

/// Read out HW state using the ICL/TGL or ADL-P callback table.
// upstream: intel_tc.c tc_phy_get_hw_state()
pub fn tc_phy_get_hw_state(io: &mut impl TcStateIo, state: &mut TcPortState) -> Result<(), Error> {
    match state.family {
        TcPhyFamily::Icl | TcPhyFamily::TigerLake => icl_tc_phy_get_hw_state(io, state),
        TcPhyFamily::AlderLakeP => adlp_tc_phy_get_hw_state(io, state),
    }
}

/// DPLL class and PHY ownership must agree for i915 to consider the TC PHY
/// connected: display-owned uses MG PHY; not-owned uses the TBT/default PLL.
// upstream: intel_tc.c tc_phy_is_connected()
pub fn tc_phy_is_connected(
    io: &mut impl TcStateIo,
    state: &TcPortState,
    pll: TcPllType,
) -> Result<bool, Error> {
    let ready = tc_phy_is_ready(io, state)?;
    let owned = tc_phy_is_owned(io, state)?;
    let display_owned = tc_phy_owned_by_display(io, ready, owned);
    Ok(if display_owned {
        pll == TcPllType::MgPhy
    } else {
        pll == TcPllType::TbtDefault
    })
}

/// Connect in the HPD-selected target mode; if unavailable, retry the port's
/// platform default exactly once before reporting the failed handoff.
// upstream: intel_tc.c tc_phy_connect()
pub fn tc_phy_connect(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
    required_lanes: u8,
) -> Result<bool, Error> {
    let live_status = tc_phy_hpd_live_status(io, state)?;
    state.legacy_port = tc_port_fixup_legacy_flag(state.legacy_port, live_status);
    state.mode = hpd_mask_to_target_mode(state, live_status);

    let mut connected = match state.family {
        TcPhyFamily::Icl | TcPhyFamily::TigerLake => icl_tc_phy_connect(io, state, required_lanes)?,
        TcPhyFamily::AlderLakeP => adlp_tc_phy_connect(io, state, required_lanes)?,
    };
    if !connected && state.mode != default_tc_mode(state) {
        state.mode = default_tc_mode(state);
        connected = match state.family {
            TcPhyFamily::Icl | TcPhyFamily::TigerLake => {
                icl_tc_phy_connect(io, state, required_lanes)?
            }
            TcPhyFamily::AlderLakeP => adlp_tc_phy_connect(io, state, required_lanes)?,
        };
    }
    if !connected {
        io.diagnostic(TcDiagnostic::ConnectFailed);
    }
    Ok(connected)
}

/// Disconnect an active PHY through its family callback and then mark it idle.
// upstream: intel_tc.c tc_phy_disconnect()
pub fn tc_phy_disconnect(io: &mut impl TcStateIo, state: &mut TcPortState) -> Result<(), Error> {
    if state.mode != TcPortMode::Disconnected {
        match state.family {
            TcPhyFamily::Icl | TcPhyFamily::TigerLake => icl_tc_phy_disconnect(io, state)?,
            TcPhyFamily::AlderLakeP => adlp_tc_phy_disconnect(io, state)?,
        }
        state.mode = TcPortMode::Disconnected;
    }
    Ok(())
}

/// Reset mode disconnects first and optionally performs a new handoff.
// upstream: intel_tc.c intel_tc_port_reset_mode()
pub fn intel_tc_port_reset_mode(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
    required_lanes: u8,
    force_disconnect: bool,
) -> Result<(), Error> {
    let old_mode = state.mode;
    io.power_flush_work();
    if state.cold_off_domain() != state.legacy_aux_domain && io.aux_powered(state.legacy_aux_domain)
    {
        io.diagnostic(TcDiagnostic::UnexpectedAuxPower);
    }
    tc_phy_disconnect(io, state)?;
    if !force_disconnect {
        let connected = tc_phy_connect(io, state, required_lanes)?;
        if !connected {
            return Err(Error::Refused);
        }
    }
    io.diagnostic(TcDiagnostic::ModeReset {
        old: old_mode,
        new: state.mode,
        pin: pin_assignment_name(state.pin_assignment),
        max_lanes: state.max_lane_count,
    });
    Ok(())
}

/// Compare HPD-selected target mode with the currently retained mode.
// upstream: intel_tc.c intel_tc_port_needs_reset()
pub fn intel_tc_port_needs_reset(
    io: &mut impl TcStateIo,
    state: &TcPortState,
) -> Result<bool, Error> {
    Ok(tc_phy_get_target_mode(io, state)? != state.mode)
}

/// Reset mode on force-disconnect or when live status requests another mode.
// upstream: intel_tc.c intel_tc_port_update_mode()
pub fn intel_tc_port_update_mode(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
    required_lanes: u8,
    force_disconnect: bool,
) -> Result<(), Error> {
    if force_disconnect || intel_tc_port_needs_reset(io, state)? {
        intel_tc_port_reset_mode(io, state, required_lanes, force_disconnect)?;
    }
    Ok(())
}

/// Increment the link refcount while the port mutex is held.
// upstream: intel_tc.c __intel_tc_port_get_link()
pub fn __intel_tc_port_get_link(state: &mut TcPortState) {
    state.link_refcount += 1;
}

/// Decrement the link refcount while the port mutex is held.
// upstream: intel_tc.c __intel_tc_port_put_link()
pub fn __intel_tc_port_put_link(state: &mut TcPortState) {
    state.link_refcount -= 1;
}

/// Read DDI_BUF_CTL's port-enable bit under the required TC-lanes domain.
// upstream: intel_tc.c tc_port_is_enabled()
pub fn tc_port_is_enabled(io: &mut impl TcStateIo, state: &TcPortState) -> Result<bool, Error> {
    assert_tc_port_power_enabled(io, state.port)?;
    io.port_enabled(state.port)
}

/// Read out the current Type-C mode and hold it until sanitize-mode.
// upstream: intel_tc.c intel_tc_port_init_mode()
pub fn intel_tc_port_init_mode(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<(), Error> {
    io.lock_port(state.port)?;
    let result = (|| {
        if state.mode != TcPortMode::Disconnected || state.lock_wakeref || state.link_refcount != 0
        {
            io.diagnostic(TcDiagnostic::UnexpectedInitialState);
        }
        tc_phy_get_hw_state(io, state)?;
        state.init_mode = state.mode;

        let update_mode = if !tc_port_is_enabled(io, state)? {
            true
        } else if state.mode == TcPortMode::Disconnected {
            if !state.legacy_port {
                io.diagnostic(TcDiagnostic::UnexpectedInitialState);
            }
            io.diagnostic(TcDiagnostic::EnabledButDisconnectedLegacy);
            true
        } else {
            false
        };

        if update_mode {
            intel_tc_port_update_mode(io, state, 1, false)?;
        }
        __intel_tc_port_get_link(state);
        Ok(())
    })();
    io.unlock_port(state.port);
    result
}

/// Active stream source preserves MST stream count versus the single-CRTC case.
// upstream: intel_tc.c tc_port_has_active_streams()
pub fn tc_port_has_active_streams(
    io: &mut impl TcStateIo,
    state: &TcPortState,
) -> Result<u32, Error> {
    let (active_streams, pll) = match io.stream_state(state)? {
        TcStreamState::Mst(active_streams) => (active_streams, TcPllType::TbtDefault),
        TcStreamState::Crtc { active, pll } => (if active { 1 } else { 0 }, pll),
        TcStreamState::Disabled => (0, TcPllType::TbtDefault),
    };
    if active_streams != 0 && !tc_phy_is_connected(io, state, pll)? {
        io.diagnostic(TcDiagnostic::ActiveStreamsOnDisconnectedPhy(active_streams));
    }
    Ok(active_streams)
}

/// Sanitize the init/resume mode: retain an enabled active PHY, otherwise
/// disconnect it and drop the initialization link reference.
// upstream: intel_tc.c intel_tc_port_sanitize_mode()
pub fn intel_tc_port_sanitize_mode(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<(), Error> {
    io.lock_port(state.port)?;
    let result: Result<(), Error> = (|| {
        if state.link_refcount != 1 {
            io.diagnostic(TcDiagnostic::UnexpectedInitialState);
        }
        if tc_port_has_active_streams(io, state)? == 0 {
            if state.init_mode != TcPortMode::TbtAlt && state.init_mode != TcPortMode::Disconnected
            {
                io.diagnostic(TcDiagnostic::DisabledPortLeftInMode(state.init_mode));
            }
            tc_phy_disconnect(io, state)?;
            __intel_tc_port_put_link(state);
        }
        Ok(())
    })();
    io.unlock_port(state.port);
    result
}

/// Diagnostic state traditionally printed by intel_tc_info(), while holding
/// the same Type-C mode lock as the source.
// upstream: intel_tc.c intel_tc_info()
pub fn intel_tc_info(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<TcPortInfo, Error> {
    intel_tc_port_lock(io, state)?;
    let info = TcPortInfo {
        mode: state.mode,
        pin_assignment: pin_assignment_name(state.pin_assignment),
        max_lane_count: state.max_lane_count,
    };
    intel_tc_port_unlock(io, state);
    Ok(info)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcPortInfo {
    pub mode: TcPortMode,
    pub pin_assignment: char,
    pub max_lane_count: u8,
}

/// Connected means live in the retained mode (or any live mode while idle).
// upstream: intel_tc.c intel_tc_port_connected()
pub fn intel_tc_port_connected(
    io: &mut impl TcStateIo,
    state: &TcPortState,
) -> Result<bool, Error> {
    if !io.port_lock_is_held(state.port) && state.link_refcount == 0 {
        io.diagnostic(TcDiagnostic::UnexpectedInitialState);
    }
    let mask = if state.mode == TcPortMode::Disconnected {
        u32::MAX
    } else {
        1 << (state.mode as u8)
    };
    Ok(tc_phy_hpd_live_status(io, state)? & mask != 0)
}

/// Whether a retained DP-alt link's requested mode has become stale.
// upstream: intel_tc.c __intel_tc_port_link_needs_reset()
pub fn __intel_tc_port_link_needs_reset(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<bool, Error> {
    io.lock_port(state.port)?;
    let result: Result<bool, Error> = (|| {
        Ok(state.link_refcount != 0
            && state.mode == TcPortMode::DpAlt
            && intel_tc_port_needs_reset(io, state)?)
    })();
    io.unlock_port(state.port);
    result
}

/// Public link-reset query; this state object represents only TC encoders.
// upstream: intel_tc.c intel_tc_port_link_needs_reset()
pub fn intel_tc_port_link_needs_reset(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<bool, Error> {
    __intel_tc_port_link_needs_reset(io, state)
}

/// Lock the connection mutex, inspect active pipes, mark connector changes,
/// recheck reset need, and commit the atomic state.
// upstream: intel_tc.c reset_link_commit()
pub fn reset_link_commit(io: &mut impl TcStateIo, state: &mut TcPortState) -> Result<(), Error> {
    io.connection_mutex_lock(state.port)?;
    let pipe_mask = io.active_pipe_mask(state.port)?;
    if pipe_mask == 0 {
        return Ok(());
    }
    io.mark_connectors_changed(state.port, pipe_mask)?;
    if !__intel_tc_port_link_needs_reset(io, state)? {
        return Ok(());
    }
    io.atomic_commit(state.port)
}

/// Allocate an internal atomic commit state and run reset-link-commit under
/// intel_modeset_lock_ctx_retry(), always releasing framework state afterward.
// upstream: intel_tc.c reset_link()
pub fn reset_link(io: &mut impl TcStateIo, state: &mut TcPortState) -> Result<(), Error> {
    io.with_atomic_modeset_retry(state.port, |io| reset_link_commit(io, state))
}

/// Delayed DP-alt link reset work serializes through the modeset mutex.
// upstream: intel_tc.c intel_tc_port_link_reset_work()
pub fn intel_tc_port_link_reset_work(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<(), Error> {
    if !__intel_tc_port_link_needs_reset(io, state)? {
        return Ok(());
    }
    io.modeset_mutex_lock(state.port)?;
    let result = reset_link(io, state);
    if result.is_err() {
        io.diagnostic(TcDiagnostic::LinkResetFailed);
    }
    io.modeset_mutex_unlock(state.port);
    result
}

/// Queue the source's 2-second debounce only if a DP-alt link needs resetting.
// upstream: intel_tc.c intel_tc_port_link_reset()
pub fn intel_tc_port_link_reset(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<bool, Error> {
    if !intel_tc_port_link_needs_reset(io, state)? {
        return Ok(false);
    }
    io.queue_link_reset_work(state.port, 2_000);
    Ok(true)
}

/// Cancel link-reset work for this TC port.
// upstream: intel_tc.c intel_tc_port_link_cancel_reset_work()
pub fn intel_tc_port_link_cancel_reset_work(io: &mut impl TcStateIo, state: &TcPortState) {
    io.cancel_link_reset_work(state.port, false);
}

/// Lock the mode, cancel pending disconnect, update an idle port, and assert
/// that the resulting mode and PHY ownership are valid.
// upstream: intel_tc.c __intel_tc_port_lock()
pub fn __intel_tc_port_lock(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
    required_lanes: u8,
) -> Result<(), Error> {
    io.lock_port(state.port)?;
    io.cancel_disconnect_work(state.port);
    if state.link_refcount == 0 {
        if let Err(error) = intel_tc_port_update_mode(io, state, required_lanes, false) {
            io.unlock_port(state.port);
            return Err(error);
        }
    }
    if state.mode == TcPortMode::Disconnected {
        io.diagnostic(TcDiagnostic::UnexpectedInitialState);
    }
    if state.mode != TcPortMode::TbtAlt {
        match tc_phy_is_owned(io, state) {
            Ok(true) => {}
            Ok(false) => io.diagnostic(TcDiagnostic::UnexpectedInitialState),
            Err(error) => {
                io.unlock_port(state.port);
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Public port lock with the source's default one-lane mode requirement.
// upstream: intel_tc.c intel_tc_port_lock()
pub fn intel_tc_port_lock(io: &mut impl TcStateIo, state: &mut TcPortState) -> Result<(), Error> {
    __intel_tc_port_lock(io, state, 1)
}

/// Delayed disconnect returns PHY ownership to Type-C after the last user.
// upstream: intel_tc.c intel_tc_port_disconnect_phy_work()
pub fn intel_tc_port_disconnect_phy_work(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<(), Error> {
    io.lock_port(state.port)?;
    let result = if state.link_refcount == 0 {
        intel_tc_port_update_mode(io, state, 1, true)
    } else {
        Ok(())
    };
    io.unlock_port(state.port);
    result
}

/// Flush the delayed PHY disconnect work item.
// upstream: intel_tc.c intel_tc_port_flush_work()
pub fn intel_tc_port_flush_work(io: &mut impl TcStateIo, state: &TcPortState) {
    io.flush_disconnect_work(state.port);
}

/// Suspend drains link reset synchronously and the PHY disconnect work.
// upstream: intel_tc.c intel_tc_port_suspend()
pub fn intel_tc_port_suspend(io: &mut impl TcStateIo, state: &TcPortState) {
    io.cancel_link_reset_work(state.port, true);
    intel_tc_port_flush_work(io, state);
}

/// Unlock queues a one-second idle disconnect before releasing the mutex.
// upstream: intel_tc.c intel_tc_port_unlock()
pub fn intel_tc_port_unlock(io: &mut impl TcStateIo, state: &TcPortState) {
    if state.link_refcount == 0 && state.mode != TcPortMode::Disconnected {
        io.queue_disconnect_work(state.port, 1_000);
    }
    io.unlock_port(state.port);
}

/// A TC port is referenced while its mutex is held or its link count is nonzero.
// upstream: intel_tc.c intel_tc_port_ref_held()
pub fn intel_tc_port_ref_held(io: &impl TcStateIo, state: &TcPortState) -> bool {
    io.port_lock_is_held(state.port) || state.link_refcount != 0
}

/// Acquire a link reference, updating mode only when this is the first user.
// upstream: intel_tc.c intel_tc_port_get_link()
pub fn intel_tc_port_get_link(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
    required_lanes: u8,
) -> Result<(), Error> {
    __intel_tc_port_lock(io, state, required_lanes)?;
    __intel_tc_port_get_link(state);
    intel_tc_port_unlock(io, state);
    Ok(())
}

/// Drop a link reference, then synchronously flush the delayed disconnect so
/// firmware can update HPD status on other DP-alt ports in a timely manner.
// upstream: intel_tc.c intel_tc_port_put_link()
pub fn intel_tc_port_put_link(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
) -> Result<(), Error> {
    intel_tc_port_lock(io, state)?;
    __intel_tc_port_put_link(state);
    intel_tc_port_unlock(io, state);
    intel_tc_port_flush_work(io, state);
    Ok(())
}

/// Select the supported display-12/13 PHY callback family, initialize its FIA
/// mapping and read/sanitize the initial mode. Display-14+ is out of scope.
// upstream: intel_tc.c intel_tc_port_init()
pub fn intel_tc_port_init(
    io: &mut impl TcStateIo,
    state: &mut TcPortState,
    display_version: u8,
    is_legacy: bool,
) -> Result<(), Error> {
    state.family = match display_version {
        12 => TcPhyFamily::TigerLake,
        13 => TcPhyFamily::AlderLakeP,
        _ => return Err(Error::UnsupportedVersion),
    };
    state.legacy_port = is_legacy;
    state.mode = TcPortMode::Disconnected;
    state.link_refcount = 0;
    tc_phy_init(io, state)?;
    intel_tc_port_init_mode(io, state)
}

/// Suspend work before the enclosing Rust owner drops its TC state and name.
/// Allocation/freeing themselves belong to the host's object-lifetime layer.
// upstream: intel_tc.c intel_tc_port_cleanup()
pub fn intel_tc_port_cleanup(io: &mut impl TcStateIo, state: &TcPortState) {
    intel_tc_port_suspend(io, state);
}
