//! Watchdog, media-change and ioctl lifecycle glue from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC).
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c) 2014
//! Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogAction {
    NicError,
    DumpDriverStatus,
    ScheduleInit,
    Net80211Watchdog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogOutcome {
    Continue { interface_timer: u8, errors: u64 },
    TimedOut { errors: u64 },
}

/// Decrement each active TX queue's independent watchdog, then perform timeout recovery.
// upstream: if_iwx.c iwx_watchdog()
pub fn watchdog_tick(
    timers: &mut [u8],
    interface_running: bool,
    output_errors: &mut u64,
    debug: bool,
    shutdown: bool,
    mut execute: impl FnMut(WatchdogAction),
) -> WatchdogOutcome {
    let mut interface_timer = 0;
    if interface_running {
        for timer in timers {
            if *timer == 0 {
                continue;
            }
            *timer -= 1;
            if *timer == 0 {
                if debug {
                    execute(WatchdogAction::NicError);
                    execute(WatchdogAction::DumpDriverStatus);
                }
                if !shutdown {
                    execute(WatchdogAction::ScheduleInit);
                }
                *output_errors += 1;
                return WatchdogOutcome::TimedOut {
                    errors: *output_errors,
                };
            }
            interface_timer = 1;
        }
    }
    execute(WatchdogAction::Net80211Watchdog);
    WatchdogOutcome::Continue {
        interface_timer,
        errors: *output_errors,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoctlKind {
    SetIfAddress,
    SetIfFlags,
    SetPower,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoctlAction {
    AcquireWriteLock,
    ReleaseWriteLock,
    EnterNetworkLevel,
    LeaveNetworkLevel,
    SetInterfaceUp,
    ClearFirmwareStatus,
    Initialize,
    Stop,
    SetPowerPolicy { level: u8 },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkIoctlResult {
    Success,
    EnetReset,
}
#[derive(Debug, PartialEq, Eq)]
pub enum IoctlError<E> {
    Operation(E),
    StaleGeneration,
    Network(E),
}

/// Serialize mutating ioctls against init/stop and preserve the ENETRESET restart path.
// upstream: if_iwx.c iwx_ioctl()
pub fn process_ioctl<E>(
    kind: IoctlKind,
    generation: u32,
    current_generation: u32,
    interface_up: &mut bool,
    interface_running: &mut bool,
    in_run_state: bool,
    power_management: bool,
    mut action: impl FnMut(IoctlAction) -> Result<(), E>,
    mut network_ioctl: impl FnMut() -> Result<NetworkIoctlResult, E>,
) -> Result<(), IoctlError<E>> {
    action(IoctlAction::AcquireWriteLock).map_err(IoctlError::Operation)?;
    if generation != current_generation {
        action(IoctlAction::ReleaseWriteLock).map_err(IoctlError::Operation)?;
        return Err(IoctlError::StaleGeneration);
    }
    let operation = (|| -> Result<(), IoctlError<E>> {
        action(IoctlAction::EnterNetworkLevel).map_err(IoctlError::Operation)?;
        let mut net_result = NetworkIoctlResult::Success;
        let mut effective_kind = kind;
        if effective_kind == IoctlKind::SetIfAddress {
            *interface_up = true;
            action(IoctlAction::SetInterfaceUp).map_err(IoctlError::Operation)?;
            effective_kind = IoctlKind::SetIfFlags;
        }
        match effective_kind {
            IoctlKind::SetIfFlags if *interface_up && !*interface_running => {
                action(IoctlAction::ClearFirmwareStatus).map_err(IoctlError::Operation)?;
                action(IoctlAction::Initialize).map_err(IoctlError::Operation)?;
            }
            IoctlKind::SetIfFlags if !*interface_up && *interface_running => {
                action(IoctlAction::Stop).map_err(IoctlError::Operation)?;
            }
            IoctlKind::SetIfFlags => {}
            IoctlKind::SetPower | IoctlKind::Other => {
                net_result = network_ioctl().map_err(IoctlError::Network)?;
                if effective_kind == IoctlKind::SetPower
                    && net_result == NetworkIoctlResult::EnetReset
                {
                    if in_run_state {
                        action(IoctlAction::SetPowerPolicy {
                            level: if power_management { 3 } else { 0 },
                        })
                        .map_err(IoctlError::Operation)?;
                    }
                }
            }
            IoctlKind::SetIfAddress => unreachable!(),
        }
        if net_result == NetworkIoctlResult::EnetReset
            && effective_kind != IoctlKind::SetPower
            && *interface_up
            && *interface_running
        {
            action(IoctlAction::Stop).map_err(IoctlError::Operation)?;
            action(IoctlAction::Initialize).map_err(IoctlError::Operation)?;
        }
        Ok(())
    })();
    let leave = action(IoctlAction::LeaveNetworkLevel).map_err(IoctlError::Operation);
    let release = action(IoctlAction::ReleaseWriteLock).map_err(IoctlError::Operation);
    operation?;
    leave?;
    release
}

/// Apply a media change, restarting only when the interface is both up and running.
// upstream: if_iwx.c iwx_media_change()
pub fn media_change<E>(
    interface_up: bool,
    interface_running: bool,
    mut ieee80211_media_change: impl FnMut() -> Result<NetworkIoctlResult, E>,
    mut stop_and_init: impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    match ieee80211_media_change()? {
        NetworkIoctlResult::Success => Ok(()),
        NetworkIoctlResult::EnetReset if interface_up && interface_running => stop_and_init(),
        NetworkIoctlResult::EnetReset => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn watchdog_tracks_per_queue_expiry_and_debug_diagnostics() {
        let mut timers = [0, 2, 1];
        let mut errors = 4;
        let mut actions = Vec::new();
        assert_eq!(
            watchdog_tick(&mut timers, true, &mut errors, true, false, |a| actions
                .push(a)),
            WatchdogOutcome::TimedOut { errors: 5 }
        );
        assert_eq!(
            actions,
            [
                WatchdogAction::NicError,
                WatchdogAction::DumpDriverStatus,
                WatchdogAction::ScheduleInit
            ]
        );
        assert_eq!(timers, [0, 1, 0]);
        let mut timers = [3];
        actions.clear();
        assert_eq!(
            watchdog_tick(&mut timers, true, &mut errors, false, false, |a| actions
                .push(a)),
            WatchdogOutcome::Continue {
                interface_timer: 1,
                errors: 5
            }
        );
        assert_eq!(actions, [WatchdogAction::Net80211Watchdog]);
    }

    #[test]
    fn ioctl_generation_guard_power_policy_and_enetreset_restart() {
        let mut up = true;
        let mut running = true;
        let mut actions = Vec::new();
        assert_eq!(
            process_ioctl(
                IoctlKind::SetPower,
                1,
                1,
                &mut up,
                &mut running,
                true,
                true,
                |a| {
                    actions.push(a);
                    Ok::<_, ()>(())
                },
                || Ok(NetworkIoctlResult::EnetReset)
            ),
            Ok(())
        );
        assert!(actions.contains(&IoctlAction::SetPowerPolicy { level: 3 }));
        assert!(!actions.contains(&IoctlAction::Stop));
        actions.clear();
        assert_eq!(
            process_ioctl(
                IoctlKind::Other,
                1,
                1,
                &mut up,
                &mut running,
                false,
                false,
                |a| {
                    actions.push(a);
                    Ok::<_, ()>(())
                },
                || Ok(NetworkIoctlResult::EnetReset)
            ),
            Ok(())
        );
        assert!(actions.contains(&IoctlAction::Stop) && actions.contains(&IoctlAction::Initialize));
        actions.clear();
        assert_eq!(
            process_ioctl(
                IoctlKind::Other,
                1,
                2,
                &mut up,
                &mut running,
                false,
                false,
                |a| {
                    actions.push(a);
                    Ok::<_, ()>(())
                },
                || Ok(NetworkIoctlResult::Success)
            ),
            Err(IoctlError::StaleGeneration)
        );
        assert_eq!(
            actions,
            [IoctlAction::AcquireWriteLock, IoctlAction::ReleaseWriteLock]
        );
        actions.clear();
        let mut not_running = false;
        assert_eq!(
            process_ioctl(
                IoctlKind::SetIfFlags,
                1,
                1,
                &mut up,
                &mut not_running,
                false,
                false,
                |a| {
                    actions.push(a);
                    if a == IoctlAction::Initialize {
                        Err("init")
                    } else {
                        Ok(())
                    }
                },
                || Ok(NetworkIoctlResult::Success)
            ),
            Err(IoctlError::Operation("init"))
        );
        assert_eq!(actions.last(), Some(&IoctlAction::ReleaseWriteLock));
    }

    #[test]
    fn media_change_restarts_only_for_up_running_interface() {
        let mut restarted = false;
        assert_eq!(
            media_change(
                true,
                true,
                || Ok::<_, ()>(NetworkIoctlResult::EnetReset),
                || {
                    restarted = true;
                    Ok(())
                }
            ),
            Ok(())
        );
        assert!(restarted);
        restarted = false;
        assert_eq!(
            media_change(
                true,
                false,
                || Ok::<_, ()>(NetworkIoctlResult::EnetReset),
                || {
                    restarted = true;
                    Ok(())
                }
            ),
            Ok(())
        );
        assert!(!restarted);
    }
}
