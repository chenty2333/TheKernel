//! Beacon-miss probing decisions from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeaconMissState {
    pub station_mode: bool,
    pub interface_running: bool,
    pub run_state: bool,
    pub missed_threshold: u32,
    pub management_timer: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeaconMissAction {
    Ignore,
    SendDirectedProbe { consecutive_missed: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeaconMissError {
    Truncated,
}

/// Inspect a missed-beacon notification and probe before the upper state machine scans.
// upstream: if_iwx.c iwx_rx_bmiss()
pub fn missed_beacon_action(
    state: BeaconMissState,
    payload: &[u8],
) -> Result<BeaconMissAction, BeaconMissError> {
    if payload.len() < 8 {
        return Err(BeaconMissError::Truncated);
    }
    if !state.station_mode || !state.interface_running || !state.run_state {
        return Ok(BeaconMissAction::Ignore);
    }
    let missed = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    if missed > state.missed_threshold && state.management_timer == 0 {
        Ok(BeaconMissAction::SendDirectedProbe {
            consecutive_missed: missed,
        })
    } else {
        Ok(BeaconMissAction::Ignore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_miss_probes_only_station_in_run_after_strict_threshold() {
        let state = BeaconMissState {
            station_mode: true,
            interface_running: true,
            run_state: true,
            missed_threshold: 10,
            management_timer: 0,
        };
        let mut payload = [0; 8];
        payload[4..8].copy_from_slice(&11u32.to_le_bytes());
        assert_eq!(
            missed_beacon_action(state, &payload),
            Ok(BeaconMissAction::SendDirectedProbe {
                consecutive_missed: 11
            })
        );
        payload[4..8].copy_from_slice(&10u32.to_le_bytes());
        assert_eq!(
            missed_beacon_action(state, &payload),
            Ok(BeaconMissAction::Ignore)
        );
        assert_eq!(
            missed_beacon_action(
                BeaconMissState {
                    management_timer: 1,
                    ..state
                },
                &payload,
            ),
            Ok(BeaconMissAction::Ignore)
        );
    }
}
