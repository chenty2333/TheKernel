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

/// Restore the active channel bitmap and prime the ANY-channel BSS sentinel.
// upstream: ieee80211_node.c ieee80211_reset_scan()
pub fn reset_scan_channels(
    active: &[bool],
    pending: &mut [bool],
    current: &mut usize,
    bss_channel_is_any: bool,
) -> Result<(), ScanError> {
    if active.is_empty() {
        return Err(ScanError::EmptyChannelSet);
    }
    if active.len() != pending.len() {
        return Err(ScanError::PendingSetLengthMismatch);
    }
    if !bss_channel_is_any && *current >= active.len() {
        return Err(ScanError::CurrentChannelOutOfRange);
    }
    pending.copy_from_slice(active);
    if bss_channel_is_any {
        *current = active.len() - 1;
    }
    Ok(())
}

pub const BGSCAN_FAIL_MAX: u16 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndScanPolicy {
    pub background_scan: bool,
    pub scan_count: u32,
    pub next_mode_is_auto: bool,
    pub scan_all_bands: bool,
    pub background_failures: u16,
    pub selected_bssid: Option<[u8; 6]>,
    pub current_bssid: Option<[u8; 6]>,
    pub selected_rssi_acceptable: bool,
    pub roam_argument_allocated: bool,
    pub driver_flush_callback: bool,
    pub current_bss_referenced: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EndScanEffects {
    pub clear_active_scan: bool,
    pub clean_inactive_nodes: bool,
    pub reset_scan: bool,
    pub increment_scan_count: bool,
    pub clear_background_scan: bool,
    pub background_failures: u16,
    pub restore_current_mode: bool,
    pub join_bssid: Option<[u8; 6]>,
    pub set_tx_management_only: bool,
    pub flush_tx_before_roam: bool,
    pub invoke_background_done: bool,
    pub retain_roam_argument_until_tx_drain: bool,
}

/// Finish the station-mode end-of-scan selection and expose driver/network effects.
// upstream: ieee80211_node.c ieee80211_end_scan()
pub fn end_station_scan(
    policy: EndScanPolicy,
    select_bss: impl FnOnce() -> (Option<[u8; 6]>, Option<[u8; 6]>),
) -> EndScanEffects {
    let (selected_bssid, current_bssid) = select_bss();
    let mut effects = EndScanEffects {
        clear_active_scan: policy.scan_count != 0,
        clean_inactive_nodes: true,
        background_failures: policy.background_failures,
        ..EndScanEffects::default()
    };
    if !policy.background_scan {
        if let Some(bssid) = selected_bssid {
            effects.join_bssid = Some(bssid);
        } else {
            effects.reset_scan = true;
            effects.increment_scan_count = policy.next_mode_is_auto || policy.scan_all_bands;
        }
        return effects;
    }

    if selected_bssid.is_none() || current_bssid.is_none() {
        effects.clear_background_scan = true;
        effects.reset_scan = true;
        effects.increment_scan_count = policy.next_mode_is_auto || policy.scan_all_bands;
        return effects;
    }
    if selected_bssid == current_bssid || !policy.selected_rssi_acceptable {
        if effects.background_failures < BGSCAN_FAIL_MAX {
            effects.background_failures = if effects.background_failures == 0 {
                1
            } else {
                effects
                    .background_failures
                    .saturating_mul(2)
                    .min(BGSCAN_FAIL_MAX)
            };
        }
        effects.clear_background_scan = true;
        effects.restore_current_mode = true;
        return effects;
    }
    if !policy.roam_argument_allocated {
        effects.clear_background_scan = true;
        return effects;
    }
    effects.background_failures = 0;
    effects.set_tx_management_only = true;
    effects.flush_tx_before_roam = policy.driver_flush_callback;
    effects.invoke_background_done = policy.driver_flush_callback;
    effects.retain_roam_argument_until_tx_drain =
        !policy.driver_flush_callback && policy.current_bss_referenced;
    effects.join_bssid = Some(selected_bssid.expect("checked selected BSS"));
    effects
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

    #[test]
    fn reset_scan_restores_active_mask_and_any_channel_starts_before_first_entry() {
        let active = [true, false, true, true];
        let mut pending = [false; 4];
        let mut current = 1;
        let channels = [NetChannel::default(); 4];
        reset_scan_channels(&active, &mut pending, &mut current, true).unwrap();
        assert_eq!(pending, active);
        assert_eq!(current, active.len() - 1);
        assert_eq!(
            next_scan_channel(&channels, &mut pending, active.len() - 1, false),
            Ok(ScanStep::NextChannel(0))
        );
    }

    #[test]
    fn station_end_scan_handles_restart_same_ap_backoff_and_roam_flush() {
        let none = end_station_scan(
            EndScanPolicy {
                background_scan: false,
                scan_count: 1,
                next_mode_is_auto: true,
                scan_all_bands: false,
                background_failures: 0,
                selected_bssid: None,
                current_bssid: None,
                selected_rssi_acceptable: false,
                roam_argument_allocated: false,
                driver_flush_callback: false,
                current_bss_referenced: false,
            },
            || (None, None),
        );
        assert!(none.clear_active_scan);
        assert!(none.clean_inactive_nodes);
        assert!(none.reset_scan && none.increment_scan_count);

        let bssid = [2, 0, 0, 0, 0, 1];
        let keep = end_station_scan(
            EndScanPolicy {
                background_scan: true,
                background_failures: 1,
                selected_rssi_acceptable: true,
                ..EndScanPolicy {
                    background_scan: false,
                    scan_count: 0,
                    next_mode_is_auto: false,
                    scan_all_bands: false,
                    background_failures: 0,
                    selected_bssid: None,
                    current_bssid: None,
                    selected_rssi_acceptable: false,
                    roam_argument_allocated: false,
                    driver_flush_callback: false,
                    current_bss_referenced: false,
                }
            },
            || (Some(bssid), Some(bssid)),
        );
        assert!(keep.clear_background_scan && keep.restore_current_mode);
        assert_eq!(keep.background_failures, 2);

        let roam = end_station_scan(
            EndScanPolicy {
                background_scan: true,
                selected_rssi_acceptable: true,
                roam_argument_allocated: true,
                driver_flush_callback: true,
                ..EndScanPolicy {
                    background_scan: false,
                    scan_count: 0,
                    next_mode_is_auto: false,
                    scan_all_bands: false,
                    background_failures: 4,
                    selected_bssid: None,
                    current_bssid: None,
                    selected_rssi_acceptable: false,
                    roam_argument_allocated: false,
                    driver_flush_callback: false,
                    current_bss_referenced: false,
                }
            },
            || (Some([2, 0, 0, 0, 0, 2]), Some(bssid)),
        );
        assert!(roam.set_tx_management_only && roam.flush_tx_before_roam);
        assert!(roam.invoke_background_done);
        assert_eq!(roam.background_failures, 0);
    }
}
