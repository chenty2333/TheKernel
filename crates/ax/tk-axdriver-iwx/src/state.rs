//! Association and disassociation command ordering from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC).
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c) 2014
//! Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssociationStep {
    AddPhy,
    ConfigureRlc,
    AddMac,
    AddBinding,
    AddStation,
    EnableMonitorQueue,
    EnableManagementQueue,
    ClearStatistics,
    ProtectSession {
        duration_tu: u32,
    },
    RemoveStation,
    DisableManagementQueue,
    RemoveBinding,
    RemoveMac,
    RemovePhy,
    UnprotectSession,
    UpdatePhy {
        chains_static: u8,
        chains_dynamic: u8,
        sco: u8,
        vht_width: u8,
    },
    UpdateStation,
    AssociateMac,
    ConfigureSpectrumFullOn,
    AllowMulticast,
    SetPowerPolicy {
        level: u8,
        asynchronous: bool,
    },
    InitializeRates,
    FlushStation,
    StopRxBa {
        tid: u8,
    },
    ConfigureSpectrumInitOff,
    DisableBeaconFilter,
    DisassociateMac,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssociationState {
    pub generation: u32,
    pub phy_active: bool,
    pub mac_active: bool,
    pub binding_active: bool,
    pub station_active: bool,
    pub management_queue_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthRequest {
    pub generation: u32,
    pub monitor_mode: bool,
    pub beacon_interval_tu: u16,
    pub rlc_api_v2: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError<E> {
    Command(AssociationStep, E),
}

/// Add PHY/MAC/binding/station contexts and roll them back in reverse order.
// upstream: if_iwx.c iwx_auth()
pub fn authenticate<E>(
    state: &mut AssociationState,
    request: AuthRequest,
    mut execute: impl FnMut(AssociationStep) -> Result<(), E>,
) -> Result<(), AuthError<E>> {
    let mut completed = Vec::new();
    let mut steps = alloc::vec![AssociationStep::AddPhy];
    if request.rlc_api_v2 {
        steps.push(AssociationStep::ConfigureRlc);
    }
    steps.extend([
        AssociationStep::AddMac,
        AssociationStep::AddBinding,
        AssociationStep::AddStation,
    ]);
    if request.monitor_mode {
        steps.push(AssociationStep::EnableMonitorQueue);
    } else {
        steps.extend([
            AssociationStep::EnableManagementQueue,
            AssociationStep::ClearStatistics,
        ]);
    }
    let expected_generation = request.generation;
    for step in steps {
        if let Err(error) = execute(step) {
            if state.generation == expected_generation {
                for done in completed.into_iter().rev() {
                    if state.generation != expected_generation {
                        break;
                    }
                    let rollback = match done {
                        AssociationStep::EnableManagementQueue => {
                            Some(AssociationStep::DisableManagementQueue)
                        }
                        AssociationStep::AddStation => Some(AssociationStep::RemoveStation),
                        AssociationStep::AddBinding => Some(AssociationStep::RemoveBinding),
                        AssociationStep::AddMac => Some(AssociationStep::RemoveMac),
                        AssociationStep::AddPhy => Some(AssociationStep::RemovePhy),
                        _ => None,
                    };
                    if let Some(rollback) = rollback {
                        let _ = execute(rollback);
                        match rollback {
                            AssociationStep::DisableManagementQueue => {
                                state.management_queue_active = false
                            }
                            AssociationStep::RemoveStation => state.station_active = false,
                            AssociationStep::RemoveBinding => state.binding_active = false,
                            AssociationStep::RemoveMac => state.mac_active = false,
                            AssociationStep::RemovePhy => state.phy_active = false,
                            _ => {}
                        }
                    }
                }
            }
            return Err(AuthError::Command(step, error));
        }
        match step {
            AssociationStep::AddPhy => state.phy_active = true,
            AssociationStep::AddMac => state.mac_active = true,
            AssociationStep::AddBinding => state.binding_active = true,
            AssociationStep::AddStation => state.station_active = true,
            AssociationStep::EnableManagementQueue => state.management_queue_active = true,
            _ => {}
        }
        completed.push(step);
        if let AssociationStep::ClearStatistics = step {
            let duration = if request.beacon_interval_tu != 0 {
                u32::from(request.beacon_interval_tu) * 9
            } else {
                900
            };
            if let Err(error) = execute(AssociationStep::ProtectSession {
                duration_tu: duration,
            }) {
                return Err(AuthError::Command(
                    AssociationStep::ProtectSession {
                        duration_tu: duration,
                    },
                    error,
                ));
            }
        }
    }
    Ok(())
}

/// Remove active association resources in source order, stopping on first error.
// upstream: if_iwx.c iwx_deauth()
pub fn deauthenticate<E>(
    state: &mut AssociationState,
    mut execute: impl FnMut(AssociationStep) -> Result<(), E>,
) -> Result<(), AuthError<E>> {
    let _ = execute(AssociationStep::UnprotectSession);
    for (active, step) in [
        (state.station_active, AssociationStep::RemoveStation),
        (state.binding_active, AssociationStep::RemoveBinding),
        (state.mac_active, AssociationStep::RemoveMac),
        (state.phy_active, AssociationStep::RemovePhy),
    ] {
        if active {
            execute(step).map_err(|error| AuthError::Command(step, error))?;
            match step {
                AssociationStep::RemoveStation => state.station_active = false,
                AssociationStep::RemoveBinding => state.binding_active = false,
                AssociationStep::RemoveMac => state.mac_active = false,
                AssociationStep::RemovePhy => state.phy_active = false,
                _ => {}
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunRequest {
    pub monitor_mode: bool,
    pub rlc_api_v2: bool,
    pub ht: bool,
    pub vht: bool,
    pub mimo_enabled: bool,
    pub channel_allows_40mhz: bool,
    pub peer_supports_40mhz: bool,
    pub channel_allows_80mhz: bool,
    pub peer_supports_80mhz: bool,
    pub channel_allows_160mhz: bool,
    pub peer_supports_160mhz: bool,
    pub sco: u8,
    pub pm_enabled: bool,
}

/// Configure association and rate adaptation in the driver's strict RUN order.
// upstream: if_iwx.c iwx_run()
pub fn run_association<E>(
    request: RunRequest,
    tx_rate_index: &mut u8,
    tx_mcs: &mut u8,
    mut execute: impl FnMut(AssociationStep) -> Result<(), E>,
) -> Result<(), AuthError<E>> {
    let mut steps = Vec::new();
    if request.monitor_mode {
        steps.push(AssociationStep::AddPhy);
        if request.rlc_api_v2 {
            steps.push(AssociationStep::ConfigureRlc);
        }
        steps.extend([
            AssociationStep::AddMac,
            AssociationStep::AddBinding,
            AssociationStep::AddStation,
            AssociationStep::EnableMonitorQueue,
        ]);
        let chains = if request.mimo_enabled { 2 } else { 1 };
        steps.push(AssociationStep::UpdatePhy {
            chains_static: chains,
            chains_dynamic: chains,
            sco: 0,
            vht_width: 0,
        });
    } else if request.ht {
        let chains = if request.mimo_enabled { 2 } else { 1 };
        let sco = if request.channel_allows_40mhz && request.peer_supports_40mhz {
            request.sco
        } else {
            0
        };
        let vht_width =
            if request.vht && request.channel_allows_160mhz && request.peer_supports_160mhz {
                2
            } else if request.vht && request.channel_allows_80mhz && request.peer_supports_80mhz {
                1
            } else {
                0
            };
        steps.push(AssociationStep::UpdatePhy {
            chains_static: chains,
            chains_dynamic: chains,
            sco,
            vht_width,
        });
    }
    steps.extend([
        AssociationStep::UpdateStation,
        AssociationStep::AssociateMac,
        AssociationStep::ConfigureSpectrumFullOn,
        AssociationStep::AllowMulticast,
        AssociationStep::SetPowerPolicy {
            level: if request.pm_enabled { 3 } else { 0 },
            asynchronous: true,
        },
    ]);
    if !request.monitor_mode {
        steps.push(AssociationStep::InitializeRates);
    }
    for step in steps {
        if step == AssociationStep::InitializeRates {
            *tx_rate_index = 0;
            *tx_mcs = 0;
        }
        execute(step).map_err(|error| AuthError::Command(step, error))?;
    }
    Ok(())
}

/// Flush TX, stop active BA sessions, then disable spectrum filtering and MAC association.
// upstream: if_iwx.c iwx_run_stop()
pub fn stop_association<E>(
    active_ba_tids: &[bool; 16],
    mut execute: impl FnMut(AssociationStep) -> Result<(), E>,
) -> Result<(), AuthError<E>> {
    let first = AssociationStep::FlushStation;
    execute(first).map_err(|error| AuthError::Command(first, error))?;
    for (tid, active) in active_ba_tids.iter().copied().enumerate() {
        if active {
            let step = AssociationStep::StopRxBa { tid: tid as u8 };
            execute(step).map_err(|error| AuthError::Command(step, error))?;
        }
    }
    for step in [
        AssociationStep::ConfigureSpectrumInitOff,
        AssociationStep::DisableBeaconFilter,
        AssociationStep::DisassociateMac,
    ] {
        execute(step).map_err(|error| AuthError::Command(step, error))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_rolls_back_only_completed_contexts_and_respects_generation() {
        let mut state = AssociationState {
            generation: 4,
            ..AssociationState::default()
        };
        let mut steps = Vec::new();
        let result = authenticate(
            &mut state,
            AuthRequest {
                generation: 4,
                monitor_mode: false,
                beacon_interval_tu: 100,
                rlc_api_v2: true,
            },
            |step| {
                steps.push(step);
                if step == AssociationStep::EnableManagementQueue {
                    Err("failed")
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(
            result,
            Err(AuthError::Command(
                AssociationStep::EnableManagementQueue,
                "failed"
            ))
        );
        assert_eq!(
            steps,
            [
                AssociationStep::AddPhy,
                AssociationStep::ConfigureRlc,
                AssociationStep::AddMac,
                AssociationStep::AddBinding,
                AssociationStep::AddStation,
                AssociationStep::EnableManagementQueue,
                AssociationStep::RemoveStation,
                AssociationStep::RemoveBinding,
                AssociationStep::RemoveMac,
                AssociationStep::RemovePhy
            ]
        );
        assert_eq!(
            state,
            AssociationState {
                generation: 4,
                ..AssociationState::default()
            }
        );
    }

    #[test]
    fn run_monitor_stops_before_rate_setup_and_station_resets_rates_after_association() {
        let mut steps = Vec::new();
        let (mut rate, mut mcs) = (9, 7);
        assert_eq!(
            run_association(
                RunRequest {
                    monitor_mode: true,
                    rlc_api_v2: false,
                    ht: false,
                    vht: false,
                    mimo_enabled: false,
                    channel_allows_40mhz: false,
                    peer_supports_40mhz: false,
                    channel_allows_80mhz: false,
                    peer_supports_80mhz: false,
                    channel_allows_160mhz: false,
                    peer_supports_160mhz: false,
                    sco: 0,
                    pm_enabled: false
                },
                &mut rate,
                &mut mcs,
                |step| {
                    steps.push(step);
                    Ok::<_, ()>(())
                }
            ),
            Ok(())
        );
        assert_eq!(
            steps.last(),
            Some(&AssociationStep::SetPowerPolicy {
                level: 0,
                asynchronous: true
            })
        );
        assert!(!steps.contains(&AssociationStep::InitializeRates));
        let mut steps = Vec::new();
        assert_eq!(
            run_association(
                RunRequest {
                    monitor_mode: false,
                    rlc_api_v2: false,
                    ht: false,
                    vht: false,
                    mimo_enabled: false,
                    channel_allows_40mhz: false,
                    peer_supports_40mhz: false,
                    channel_allows_80mhz: false,
                    peer_supports_80mhz: false,
                    channel_allows_160mhz: false,
                    peer_supports_160mhz: false,
                    sco: 0,
                    pm_enabled: false
                },
                &mut rate,
                &mut mcs,
                |step| {
                    steps.push(step);
                    Ok::<_, ()>(())
                }
            ),
            Ok(())
        );
        assert_eq!(rate, 0);
        assert_eq!(mcs, 0);
        assert_eq!(steps.last(), Some(&AssociationStep::InitializeRates));
    }

    #[test]
    fn stop_association_flushes_before_ba_and_policy_cleanup() {
        let mut steps = Vec::new();
        let mut active = [false; 16];
        active[3] = true;
        active[9] = true;
        assert_eq!(
            stop_association(&active, |step| {
                steps.push(step);
                Ok::<_, ()>(())
            }),
            Ok(())
        );
        assert_eq!(steps[0], AssociationStep::FlushStation);
        assert_eq!(steps[1], AssociationStep::StopRxBa { tid: 3 });
        assert_eq!(steps[2], AssociationStep::StopRxBa { tid: 9 });
        assert_eq!(
            &steps[3..],
            &[
                AssociationStep::ConfigureSpectrumInitOff,
                AssociationStep::DisableBeaconFilter,
                AssociationStep::DisassociateMac
            ]
        );
    }

    #[test]
    fn deauth_unprotects_then_removes_station_binding_mac_and_phy() {
        let mut state = AssociationState {
            phy_active: true,
            mac_active: true,
            binding_active: true,
            station_active: true,
            management_queue_active: true,
            ..AssociationState::default()
        };
        let mut steps = Vec::new();
        assert_eq!(
            deauthenticate(&mut state, |step| {
                steps.push(step);
                Ok::<_, ()>(())
            }),
            Ok(())
        );
        assert_eq!(
            steps,
            [
                AssociationStep::UnprotectSession,
                AssociationStep::RemoveStation,
                AssociationStep::RemoveBinding,
                AssociationStep::RemoveMac,
                AssociationStep::RemovePhy
            ]
        );
        assert!(
            !state.phy_active
                && !state.mac_active
                && !state.binding_active
                && !state.station_active
        );
        assert!(state.management_queue_active);
    }
}
