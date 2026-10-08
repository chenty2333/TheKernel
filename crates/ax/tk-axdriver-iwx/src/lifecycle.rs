//! Interface lifecycle and multicast setup from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{CommandError, EncodedCommand, HostCommand};

pub const MCAST_FILTER_COMMAND: u8 = 0xd0;
pub const MCAST_FILTER_PAYLOAD_BYTES: usize = 12;

/// Build a pass-all multicast filter associated with the current BSSID.
// upstream: if_iwx.c iwx_allow_mcast()
pub fn allow_multicast_command(bssid: [u8; 6], slot: u8) -> Result<EncodedCommand, CommandError> {
    let mut payload = [0u8; MCAST_FILTER_PAYLOAD_BYTES];
    payload[0] = 1;
    payload[1] = 0;
    payload[2] = 0;
    payload[3] = 1;
    payload[4..10].copy_from_slice(&bssid);
    let command = HostCommand {
        id: u32::from(MCAST_FILTER_COMMAND),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitAction {
    Preinit,
    StartHardware,
    InitHardware,
    SetupHtRates,
    SetupVhtRates,
    InitializeTaskReferences,
    ClearOutputActive,
    MarkRunning,
    SelectMonitorChannel,
    EnterRunState,
    BeginScan,
    WaitForState { timeout_seconds: u8 },
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfaceInitState {
    pub generation: u32,
    pub interface_running: bool,
    pub output_active: bool,
    pub current_state: crate::state::WifiState,
    pub monitor_mode: bool,
    pub ht_rates_enabled: bool,
    pub vht_rates_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome<E> {
    Success,
    StaleGeneration,
    Failed(InitAction, E),
    Interrupted(E),
}

/// Initialize firmware, rates, and net80211 scan state using the source wait loop.
// upstream: if_iwx.c iwx_init()
pub fn initialize_interface<E>(
    state: &mut InterfaceInitState,
    mut execute: impl FnMut(InitAction) -> Result<(), E>,
    mut wait_for_scan_state: impl FnMut(&mut InterfaceInitState) -> Result<(), E>,
) -> InitOutcome<E> {
    state.generation = state.generation.wrapping_add(1);
    let generation = state.generation;
    for action in [
        InitAction::Preinit,
        InitAction::StartHardware,
        InitAction::InitHardware,
    ] {
        if let Err(error) = execute(action) {
            if action == InitAction::InitHardware && state.generation == generation {
                let _ = execute(InitAction::Stop);
            }
            return InitOutcome::Failed(action, error);
        }
    }
    if state.ht_rates_enabled {
        if let Err(error) = execute(InitAction::SetupHtRates) {
            return InitOutcome::Failed(InitAction::SetupHtRates, error);
        }
    }
    if state.vht_rates_enabled {
        if let Err(error) = execute(InitAction::SetupVhtRates) {
            return InitOutcome::Failed(InitAction::SetupVhtRates, error);
        }
    }
    for action in [
        InitAction::InitializeTaskReferences,
        InitAction::ClearOutputActive,
        InitAction::MarkRunning,
    ] {
        if let Err(error) = execute(action) {
            return InitOutcome::Failed(action, error);
        }
    }
    state.interface_running = true;
    state.output_active = false;
    if state.monitor_mode {
        for action in [InitAction::SelectMonitorChannel, InitAction::EnterRunState] {
            if let Err(error) = execute(action) {
                return InitOutcome::Failed(action, error);
            }
        }
        state.current_state = crate::state::WifiState::Run;
        return InitOutcome::Success;
    }
    if let Err(error) = execute(InitAction::BeginScan) {
        return InitOutcome::Failed(InitAction::BeginScan, error);
    }
    loop {
        if let Err(error) = execute(InitAction::WaitForState { timeout_seconds: 1 }) {
            let _ = execute(InitAction::Stop);
            return InitOutcome::Interrupted(error);
        }
        if let Err(error) = wait_for_scan_state(state) {
            let _ = execute(InitAction::Stop);
            return InitOutcome::Interrupted(error);
        }
        if state.generation != generation {
            return InitOutcome::StaleGeneration;
        }
        if state.current_state == crate::state::WifiState::Scan {
            return InitOutcome::Success;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopAction {
    SetShutdown,
    CancelInit,
    CancelNewState,
    CancelBa,
    CancelSetKey,
    ClearSetKeyQueue,
    CancelMacContext,
    CancelPhyContext,
    CancelBackgroundScanDone,
    FinalizeTaskReferences,
    MfpLeave,
    StopDevice,
    FreeBackgroundScanArgument,
    ClearCommandResponses,
    ClearRunning,
    ClearOutputActive,
    ResetNodeContext,
    ClearScanFlags,
    ClearContextFlags,
    ClearErrorAndShutdownFlags,
    ClearTxFlush,
    ClearBaState,
    ClearTxTimers,
    ClearInterfaceTimer,
    SetNet80211Init,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfaceStopState {
    pub generation: u32,
    pub shutdown: bool,
    pub running: bool,
    pub output_active: bool,
    pub station_mode: bool,
    pub current_state: crate::state::WifiState,
    pub mfp: bool,
    pub tx_flush: bool,
    pub scanning: bool,
    pub background_scanning: bool,
    pub phy_active: bool,
    pub mac_active: bool,
    pub binding_active: bool,
    pub station_active: bool,
    pub time_event_active: bool,
    pub hardware_error: bool,
    pub rx_ba_sessions: u8,
}

/// Stop device and reset software after canceling/refcount-draining all tasks.
// upstream: if_iwx.c iwx_stop()
pub fn stop_interface(state: &mut InterfaceStopState, mut execute: impl FnMut(StopAction)) {
    execute(StopAction::SetShutdown);
    state.shutdown = true;
    for action in [
        StopAction::CancelInit,
        StopAction::CancelNewState,
        StopAction::CancelBa,
        StopAction::CancelSetKey,
        StopAction::ClearSetKeyQueue,
        StopAction::CancelMacContext,
        StopAction::CancelPhyContext,
        StopAction::CancelBackgroundScanDone,
        StopAction::FinalizeTaskReferences,
    ] {
        execute(action);
    }
    if state.station_mode && state.current_state == crate::state::WifiState::Run && state.mfp {
        execute(StopAction::MfpLeave);
    }
    execute(StopAction::StopDevice);
    execute(StopAction::FreeBackgroundScanArgument);
    state.generation = state.generation.wrapping_add(1);
    execute(StopAction::ClearCommandResponses);
    execute(StopAction::ClearRunning);
    state.running = false;
    execute(StopAction::ClearOutputActive);
    state.output_active = false;
    execute(StopAction::ResetNodeContext);
    execute(StopAction::ClearScanFlags);
    state.scanning = false;
    state.background_scanning = false;
    execute(StopAction::ClearContextFlags);
    state.phy_active = false;
    state.mac_active = false;
    state.binding_active = false;
    state.station_active = false;
    state.time_event_active = false;
    execute(StopAction::ClearErrorAndShutdownFlags);
    state.hardware_error = false;
    state.shutdown = false;
    execute(StopAction::ClearTxFlush);
    state.tx_flush = false;
    execute(StopAction::ClearBaState);
    state.rx_ba_sessions = 0;
    execute(StopAction::ClearTxTimers);
    execute(StopAction::ClearInterfaceTimer);
    execute(StopAction::SetNet80211Init);
    state.current_state = crate::state::WifiState::Init;
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn multicast_filter_payload_sets_own_port_pass_all_and_bssid() {
        let command = allow_multicast_command([1, 2, 3, 4, 5, 6], 0).unwrap();
        assert_eq!(command.bytes.len(), 8 + MCAST_FILTER_PAYLOAD_BYTES);
        assert_eq!(&command.bytes[8..12], &[1, 0, 0, 1]);
        assert_eq!(&command.bytes[12..18], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(&command.bytes[18..20], &[0, 0]);
    }

    #[test]
    fn init_monitor_and_station_wait_paths_follow_source_order() {
        let mut state = InterfaceInitState {
            generation: 3,
            interface_running: false,
            output_active: true,
            current_state: crate::state::WifiState::Init,
            monitor_mode: true,
            ht_rates_enabled: true,
            vht_rates_enabled: true,
        };
        let mut actions = Vec::new();
        assert_eq!(
            initialize_interface(
                &mut state,
                |a| {
                    actions.push(a);
                    Ok::<_, ()>(())
                },
                |_| Ok::<_, ()>(())
            ),
            InitOutcome::Success
        );
        assert_eq!(state.generation, 4);
        assert_eq!(
            actions,
            [
                InitAction::Preinit,
                InitAction::StartHardware,
                InitAction::InitHardware,
                InitAction::SetupHtRates,
                InitAction::SetupVhtRates,
                InitAction::InitializeTaskReferences,
                InitAction::ClearOutputActive,
                InitAction::MarkRunning,
                InitAction::SelectMonitorChannel,
                InitAction::EnterRunState
            ]
        );
        let mut state = InterfaceInitState {
            monitor_mode: false,
            ..state
        };
        state.current_state = crate::state::WifiState::Init;
        let mut waits = 0;
        assert_eq!(
            initialize_interface(
                &mut state,
                |_| Ok::<_, ()>(()),
                |state| {
                    waits += 1;
                    if waits == 2 {
                        state.current_state = crate::state::WifiState::Scan;
                    }
                    Ok::<_, ()>(())
                }
            ),
            InitOutcome::Success
        );
    }

    #[test]
    fn stop_cancels_tasks_then_invalidates_generation_and_resets_flags() {
        let mut state = InterfaceStopState {
            generation: 5,
            shutdown: false,
            running: true,
            output_active: true,
            station_mode: true,
            current_state: crate::state::WifiState::Run,
            mfp: true,
            tx_flush: true,
            scanning: true,
            background_scanning: true,
            phy_active: true,
            mac_active: true,
            binding_active: true,
            station_active: true,
            time_event_active: true,
            hardware_error: true,
            rx_ba_sessions: 3,
        };
        let mut actions = Vec::new();
        stop_interface(&mut state, |action| actions.push(action));
        assert_eq!(actions[0], StopAction::SetShutdown);
        assert_eq!(actions[9], StopAction::FinalizeTaskReferences);
        assert_eq!(actions[10], StopAction::MfpLeave);
        assert_eq!(state.generation, 6);
        assert!(!state.shutdown && !state.running && !state.phy_active && !state.tx_flush);
        assert_eq!(state.current_state, crate::state::WifiState::Init);
    }
}
