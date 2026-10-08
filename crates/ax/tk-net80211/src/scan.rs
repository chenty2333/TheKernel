//! Channel-scan selection from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_node.c` rev 1.217 and
//! `sys/net80211/ieee80211.c` rev 1.92 (BSD-3-Clause).
//! Copyright (c) 2001 Atsushi Onoe, (c) 2002, 2003 Sam Leffler, Errno
//! Consulting, and (c) 2008 Damien Bergamini.

use crate::{NET_CHAN_PASSIVE, NetChannel};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackgroundScanPolicy {
    pub already_scanning: bool,
    pub state_running: bool,
    pub management_timer_active: bool,
    pub rsn_enabled: bool,
    pub rsn_port_valid: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackgroundScanEffects {
    pub driver_start_called: bool,
    pub started: bool,
    pub clear_scan_nodes: bool,
    pub mark_background_scanning: bool,
}

/// Apply the OpenBSD background-scan gates and run the driver's start callback.
// upstream: ieee80211.c ieee80211_begin_bgscan()
pub fn begin_background_scan(
    policy: BackgroundScanPolicy,
    start_driver_scan: impl FnOnce() -> bool,
) -> BackgroundScanEffects {
    if policy.already_scanning
        || !policy.state_running
        || policy.management_timer_active
        || (policy.rsn_enabled && !policy.rsn_port_valid)
    {
        return BackgroundScanEffects::default();
    }
    let started = start_driver_scan();
    BackgroundScanEffects {
        driver_start_called: true,
        started,
        clear_scan_nodes: started,
        mark_background_scanning: started,
    }
}

/// Timer callback entry point for the background-scan helper.
// upstream: ieee80211.c ieee80211_bgscan_timeout()
pub fn background_scan_timeout(
    policy: BackgroundScanPolicy,
    start_driver_scan: impl FnOnce() -> bool,
) -> BackgroundScanEffects {
    begin_background_scan(policy, start_driver_scan)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanProgress {
    pub active_scan: bool,
    pub active_scans: u32,
    pub passive_scans: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanError {
    EmptyChannelSet,
    PendingSetLengthMismatch,
    CurrentChannelOutOfRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanStep {
    NextChannel(usize),
    Complete,
}

/// Begin active scan except for hostap mode, which scans passively.
// upstream: ieee80211_node.c ieee80211_begin_scan()
pub fn begin_scan(progress: &mut ScanProgress, hostap_mode: bool) {
    if !hostap_mode {
        progress.active_scan = true;
        progress.active_scans += 1;
    } else {
        progress.passive_scans += 1;
    }
}

/// Pick and clear the next pending channel, wrapping once and skipping passive channels in active mode.
// upstream: ieee80211_node.c ieee80211_next_scan()
pub fn next_scan_channel(
    channels: &[NetChannel],
    pending: &mut [bool],
    current: usize,
    active_scan: bool,
) -> Result<ScanStep, ScanError> {
    if channels.is_empty() {
        return Err(ScanError::EmptyChannelSet);
    }
    if pending.len() != channels.len() {
        return Err(ScanError::PendingSetLengthMismatch);
    }
    if current >= channels.len() {
        return Err(ScanError::CurrentChannelOutOfRange);
    }
    let mut channel = current;
    loop {
        channel += 1;
        if channel >= channels.len() {
            channel = 0;
        }
        if pending[channel] && (!active_scan || channels[channel].flags & NET_CHAN_PASSIVE == 0) {
            pending[channel] = false;
            return Ok(ScanStep::NextChannel(channel));
        }
        if channel == current {
            return Ok(ScanStep::Complete);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_scan_runs_only_after_all_source_guards_and_success_clears_cache() {
        let policy = BackgroundScanPolicy {
            already_scanning: false,
            state_running: true,
            management_timer_active: false,
            rsn_enabled: true,
            rsn_port_valid: true,
        };
        let mut called = false;
        let effects = begin_background_scan(policy, || {
            called = true;
            true
        });
        assert!(called);
        assert_eq!(
            effects,
            BackgroundScanEffects {
                driver_start_called: true,
                started: true,
                clear_scan_nodes: true,
                mark_background_scanning: true,
            }
        );

        let blocked = BackgroundScanPolicy {
            rsn_port_valid: false,
            ..policy
        };
        let mut called = false;
        assert_eq!(
            background_scan_timeout(blocked, || {
                called = true;
                true
            }),
            BackgroundScanEffects::default()
        );
        assert!(!called);
    }

    #[test]
    fn begin_scan_counts_active_and_hostap_passive_runs() {
        let mut scan = ScanProgress::default();
        begin_scan(&mut scan, false);
        assert_eq!(
            scan,
            ScanProgress {
                active_scan: true,
                active_scans: 1,
                passive_scans: 0
            }
        );
        begin_scan(&mut scan, true);
        assert_eq!(scan.active_scans, 1);
        assert_eq!(scan.passive_scans, 1);
        assert!(scan.active_scan);
    }

    #[test]
    fn channel_scan_wraps_skips_passive_and_clears_only_selected_bits() {
        let channels = [
            NetChannel::default(),
            NetChannel {
                flags: 0,
                ..Default::default()
            },
            NetChannel {
                flags: NET_CHAN_PASSIVE,
                ..Default::default()
            },
            NetChannel {
                flags: 0,
                ..Default::default()
            },
        ];
        let mut pending = [true, true, true, true];
        assert_eq!(
            next_scan_channel(&channels, &mut pending, 1, true),
            Ok(ScanStep::NextChannel(3))
        );
        assert_eq!(pending, [true, true, true, false]);
        assert_eq!(
            next_scan_channel(&channels, &mut pending, 3, true),
            Ok(ScanStep::NextChannel(0))
        );
        assert_eq!(
            next_scan_channel(&channels, &mut pending, 0, true),
            Ok(ScanStep::NextChannel(1))
        );
        assert_eq!(
            next_scan_channel(&channels, &mut pending, 1, true),
            Ok(ScanStep::Complete)
        );
        assert!(pending[2]); // passive-only channel remains pending during active scan
        assert_eq!(
            next_scan_channel(&channels, &mut pending, 1, false),
            Ok(ScanStep::NextChannel(2))
        );
        assert!(!pending[2]);
    }
}
