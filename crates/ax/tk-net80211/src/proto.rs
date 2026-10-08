//! Legacy rate negotiation from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_proto.c` rev 1.176,
//! `ieee80211_node.h` rev 1.64 and `ieee80211.h` rev 1.137 (BSD-3-Clause).
//! Copyright (c) 2001 Atsushi Onoe; Copyright (c) 2002, 2003 Sam Leffler,
//! Errno Consulting; Copyright (c) 2007-2009 Damien Bergamini.

use crate::{RATE_MAX_SIZE, RATE_VALUE, RateSet};

pub const FLAG_USE_PROTECTION: u32 = 0x0010_0000;
pub const FLAG_SHORT_PREAMBLE: u32 = 0x0004_0000;
pub const FLAG_SHORT_SLOT: u32 = 0x0002_0000;
pub const CAP_SHORT_PREAMBLE: u32 = 0x0000_0100;
pub const CAP_SHORT_SLOT: u32 = 0x0000_0080;
pub const BEACON_MISS_THRESHOLD: u32 = 30;
pub const IEEE80211_DUR_TU: u32 = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ErpState {
    pub use_protection: bool,
    pub short_preamble: bool,
    pub short_slot: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenAuthState {
    pub authentication_state: bool,
    pub rsn_enabled: bool,
    pub is_bss_node: bool,
    pub sequence: u16,
    pub status: u16,
    pub auth_subtype: u8,
    pub node_failures: u32,
    pub bad_auth_count: u32,
    pub auth_fail_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolState {
    Scan,
    Auth,
    Assoc,
    Run,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalPhyConfig {
    pub modecaps: u32,
    pub ht_enabled: bool,
    pub vht_enabled: bool,
    pub he_enabled: bool,
    pub channel_is_2ghz: bool,
    pub channel_is_5ghz: bool,
    pub channel_supports_ac: bool,
    pub channel_supports_he: bool,
    pub station_mode: bool,
    pub wep_enabled: bool,
    pub rsn_enabled: bool,
    pub ht_caps: crate::HtCapabilities,
    pub supported_ht_mcs: [u8; 10],
    pub vht_caps: crate::VhtCapabilities,
    pub he_caps: crate::HeCapabilities,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerPhyConfig {
    pub ht_caps: crate::HtCapabilities,
    pub ht_operation: crate::HtOperation,
    pub vht_caps: crate::VhtCapabilities,
    pub vht_operation: crate::VhtOperation,
    pub he_caps: crate::HeCapabilities,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NegotiatedPhy {
    pub ht: bool,
    pub ht_sgi20: bool,
    pub ht_sgi40: bool,
    pub vht: bool,
    pub vht_sgi80: bool,
    pub vht_sgi160: bool,
    pub he: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlternativeBss {
    pub selected_index: usize,
    pub phy_mode: crate::PhyMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenAuthEffects {
    pub clear_protected_txrx: bool,
    pub clear_mgmt_protection: bool,
    pub invalidate_port: bool,
    pub reset_replay_state: bool,
    pub delete_pairwise_key: bool,
    pub try_another_bss: bool,
    pub new_state: Option<ProtocolState>,
    pub new_state_reason: Option<u8>,
}

/// Apply the station branch of Open System authentication response processing.
// upstream: ieee80211_proto.c ieee80211_auth_open()
pub fn auth_open_station(state: &mut OpenAuthState) -> OpenAuthEffects {
    if !state.authentication_state || state.sequence != 2 {
        state.bad_auth_count += 1;
        return OpenAuthEffects::default();
    }
    let mut effects = OpenAuthEffects::default();
    if state.rsn_enabled {
        effects.clear_protected_txrx = true;
        effects.clear_mgmt_protection = true;
        effects.invalidate_port = true;
        effects.reset_replay_state = true;
        effects.delete_pairwise_key = true;
    }
    if state.status != 0 {
        if state.is_bss_node {
            effects.try_another_bss = true;
        } else {
            state.node_failures += 1;
        }
        state.auth_fail_count += 1;
        return effects;
    }
    effects.new_state = Some(ProtocolState::Assoc);
    effects.new_state_reason = Some(state.auth_subtype);
    effects
}

/// Negotiate HT support, mandatory Basic MCS, encryption restrictions and SGI.
// upstream: ieee80211_proto.c ieee80211_ht_negotiate()
pub fn ht_negotiate(
    local: &LocalPhyConfig,
    peer: &PeerPhyConfig,
    pairwise_ciphers: u32,
) -> (bool, bool, bool) {
    if local.modecaps & (1 << crate::PhyMode::N as u8) == 0
        || !local.ht_enabled
        || !crate::supports_ht(&peer.ht_caps)
    {
        return (false, false, false);
    }
    if local.station_mode {
        for mcs in 0..77 {
            if peer.ht_operation.basic_mcs[mcs / 8] & (1 << (mcs % 8)) != 0
                && local.supported_ht_mcs[mcs / 8] & (1 << (mcs % 8)) == 0
            {
                return (false, false, false);
            }
        }
    }
    if local.wep_enabled
        || (local.rsn_enabled
            && pairwise_ciphers & (crate::CIPHER_USE_GROUP | crate::CIPHER_TKIP) != 0)
    {
        return (false, false, false);
    }
    (
        true,
        crate::supports_ht_sgi20(&peer.ht_caps) && local.ht_caps.caps & crate::HTCAP_SGI20 != 0,
        crate::supports_ht_sgi40(&peer.ht_caps) && local.ht_caps.caps & crate::HTCAP_SGI40 != 0,
    )
}

/// Negotiate 5-GHz VHT support and verify all peer basic MCS/NSS requirements.
// upstream: ieee80211_proto.c ieee80211_vht_negotiate()
pub fn vht_negotiate(local: &LocalPhyConfig, peer: &PeerPhyConfig) -> (bool, bool, bool) {
    if local.modecaps & (1 << crate::PhyMode::Ac as u8) == 0
        || !local.vht_enabled
        || !local.channel_is_5ghz
        || !local.channel_supports_ac
        || !crate::supports_vht(&peer.vht_caps)
    {
        return (false, false, false);
    }
    if local.station_mode {
        for nss in 0..8 {
            let shift = nss * 2;
            let basic = (peer.vht_operation.basic_mcs >> shift) & 3;
            let local_rx = (local.vht_caps.rx_mcs >> shift) & 3;
            if basic != 3 && basic > local_rx {
                return (false, false, false);
            }
        }
    }
    (
        true,
        peer.vht_caps.caps & (1 << 5) != 0 && local.vht_caps.caps & (1 << 5) != 0,
        peer.vht_caps.caps & (1 << 6) != 0 && local.vht_caps.caps & (1 << 6) != 0,
    )
}

/// Negotiate HE only when local/peer MCS, channel, HT and HE mode all agree.
// upstream: ieee80211_proto.c ieee80211_he_negotiate()
pub fn he_negotiate(local: &LocalPhyConfig, peer: &PeerPhyConfig, ht_negotiated: bool) -> bool {
    if local.modecaps & (1 << crate::PhyMode::Ax as u8) == 0
        || !local.he_enabled
        || !local.channel_supports_he
        || !local.ht_enabled
        || !ht_negotiated
        || local.he_caps.rx_mcs_80 & 3 == 3
        || !crate::supports_he(&peer.he_caps)
    {
        return false;
    }
    if local.station_mode {
        for nss in 0..8 {
            let shift = nss * 2;
            let basic = (peer.he_caps.basic_mcs >> shift) & 3;
            let local_rx = (local.he_caps.rx_mcs_80 >> shift) & 3;
            if basic != 3 && basic > local_rx {
                return false;
            }
        }
    }
    true
}

/// Mark the current AP failed and choose a different compatible BSS.
// upstream: ieee80211_proto.c ieee80211_try_another_bss()
pub fn try_another_bss(
    candidates: &mut [crate::AccessPoint],
    current_bssid: [u8; 6],
    policy: &crate::BssMatchPolicy<'_>,
    scan_all_bands: bool,
    current_mode: &mut crate::PhyMode,
    max_rssi: u8,
    fix_rate: impl FnMut(&mut crate::RateSet) -> u8,
) -> Option<AlternativeBss> {
    if let Some(current) = candidates.iter_mut().find(|ap| ap.bssid == current_bssid) {
        current.previous_failures = current.previous_failures.saturating_add(1);
    }
    if scan_all_bands {
        *current_mode = crate::PhyMode::Auto;
    }
    let selection = crate::choose_bss(
        policy,
        candidates,
        Some(current_bssid),
        scan_all_bands,
        max_rssi,
        fix_rate,
    );
    let selected_index = selection.selected?;
    if candidates[selected_index].bssid == current_bssid {
        return None;
    }
    Some(AlternativeBss {
        selected_index,
        phy_mode: *current_mode,
    })
}

pub const FIX_RATE_SORT: u32 = 0x01;
pub const FIX_RATE_FIXED: u32 = 0x02;
pub const FIX_RATE_NEGOTIATE: u32 = 0x04;
pub const FIX_RATE_DELETE: u32 = 0x08;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixRateConfig<'a> {
    pub supported_rates: &'a RateSet,
    pub fixed_rate: Option<usize>,
    pub hostap_mode: bool,
}

/// Sort, negotiate, delete unsupported rates and validate local fixed-rate policy.
// upstream: ieee80211_proto.c ieee80211_fix_rate()
pub fn fix_rate(peer_rates: &mut RateSet, config: FixRateConfig<'_>, mut flags: u32) -> u8 {
    if flags & FIX_RATE_FIXED != 0 && config.fixed_rate.is_none() {
        flags &= !FIX_RATE_FIXED;
    }
    let supported_count = config.supported_rates.count.min(RATE_MAX_SIZE);
    let peer_count = peer_rates.count.min(RATE_MAX_SIZE);
    peer_rates.count = peer_count;
    let mut ok_rate = 0;
    let mut bad_rate = 0;
    let mut fixed_rate = 0;
    let mut error = false;
    let mut index = 0;
    while index < peer_rates.count {
        if flags & FIX_RATE_SORT != 0 {
            for next in index + 1..peer_rates.count {
                if peer_rates.rates[index] & RATE_VALUE > peer_rates.rates[next] & RATE_VALUE {
                    peer_rates.rates.swap(index, next);
                }
            }
        }
        let rate = peer_rates.rates[index] & RATE_VALUE;
        bad_rate = rate;
        if flags & FIX_RATE_FIXED != 0 {
            if let Some(fixed_index) = config.fixed_rate {
                if fixed_index < supported_count
                    && rate == config.supported_rates.rates[fixed_index] & RATE_VALUE
                {
                    fixed_rate = rate;
                }
            }
        }
        let mut ignore = false;
        if flags & FIX_RATE_NEGOTIATE != 0 {
            let supported = config.supported_rates.rates[..supported_count]
                .iter()
                .find(|supported_rate| **supported_rate & RATE_VALUE == rate);
            if let Some(supported_rate) = supported {
                // Preserve the AP's Basic marker on mutually-supported rates.
                peer_rates.rates[index] = *supported_rate;
            } else {
                if config.hostap_mode && peer_rates.rates[index] & crate::LEGACY_RATE_BASIC != 0 {
                    error = true;
                }
                ignore = true;
            }
        }
        if flags & FIX_RATE_DELETE != 0 && ignore {
            peer_rates.count -= 1;
            for move_index in index..peer_rates.count {
                peer_rates.rates[move_index] = peer_rates.rates[move_index + 1];
            }
            peer_rates.rates[peer_rates.count] = 0;
            continue;
        }
        if !ignore {
            ok_rate = peer_rates.rates[index];
        }
        index += 1;
    }
    if ok_rate == 0 || error || (flags & FIX_RATE_FIXED != 0 && fixed_rate == 0) {
        bad_rate | crate::LEGACY_RATE_BASIC
    } else {
        ok_rate & RATE_VALUE
    }
}

/// Set the short-slot flag and return whether the state changed.
// upstream: ieee80211_proto.c ieee80211_set_shortslottime()
pub fn set_short_slot(flags: &mut u32, enabled: bool) -> bool {
    let old = *flags & FLAG_SHORT_SLOT != 0;
    if enabled {
        *flags |= FLAG_SHORT_SLOT;
    } else {
        *flags &= !FLAG_SHORT_SLOT;
    }
    old != enabled
}

/// Reset 11g ERP state and derive short-slot/preamble behavior from mode/band.
// upstream: ieee80211_proto.c ieee80211_reset_erp()
pub fn reset_erp(
    mode: crate::PhyMode,
    channel_is_2ghz: bool,
    channel_is_5ghz: bool,
    hostap_mode: bool,
    capabilities: u32,
) -> ErpState {
    let mode_5ghz = mode == crate::PhyMode::A || (mode == crate::PhyMode::N && channel_is_5ghz);
    let mode_2ghz_g_or_n =
        mode == crate::PhyMode::G || (mode == crate::PhyMode::N && channel_is_2ghz);
    let short_slot =
        mode_5ghz || (hostap_mode && mode_2ghz_g_or_n && capabilities & CAP_SHORT_SLOT != 0);
    let short_preamble = mode_5ghz || capabilities & CAP_SHORT_PREAMBLE != 0;
    ErpState {
        use_protection: false,
        short_preamble,
        short_slot,
    }
}

/// Scale missed-beacon threshold to the peer interval with at least one miss permitted.
// upstream: ieee80211_proto.c ieee80211_set_beacon_miss_threshold()
pub fn beacon_miss_threshold(beacon_interval_tu: u16, current_threshold: u32) -> u32 {
    let interval = u32::from(beacon_interval_tu);
    if interval == 0 {
        return current_threshold;
    }
    let timeout = (BEACON_MISS_THRESHOLD * interval)
        .min(BEACON_MISS_THRESHOLD * (IEEE80211_DUR_TU / 10))
        .max(2 * interval);
    timeout / interval
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> RateSet {
        RateSet::new(&[0x82, 0x84, 11, 22, 0x8c, 18, 24, 36])
    }

    #[test]
    fn open_auth_station_validates_sequence_clears_rsn_and_selects_next_state() {
        let mut state = OpenAuthState {
            authentication_state: false,
            rsn_enabled: true,
            is_bss_node: true,
            sequence: 1,
            status: 0,
            auth_subtype: 0xb0,
            node_failures: 0,
            bad_auth_count: 0,
            auth_fail_count: 0,
        };
        assert_eq!(auth_open_station(&mut state), OpenAuthEffects::default());
        assert_eq!(state.bad_auth_count, 1);
        state.authentication_state = true;
        state.sequence = 2;
        let effects = auth_open_station(&mut state);
        assert!(
            effects.clear_protected_txrx
                && effects.clear_mgmt_protection
                && effects.invalidate_port
                && effects.reset_replay_state
                && effects.delete_pairwise_key
        );
        assert_eq!(effects.new_state, Some(ProtocolState::Assoc));
        assert_eq!(effects.new_state_reason, Some(0xb0));
    }

    fn phy_config() -> (LocalPhyConfig, PeerPhyConfig) {
        let local = LocalPhyConfig {
            modecaps: (1 << crate::PhyMode::N as u8)
                | (1 << crate::PhyMode::Ac as u8)
                | (1 << crate::PhyMode::Ax as u8),
            ht_enabled: true,
            vht_enabled: true,
            he_enabled: true,
            channel_is_2ghz: false,
            channel_is_5ghz: true,
            channel_supports_ac: true,
            channel_supports_he: true,
            station_mode: true,
            wep_enabled: false,
            rsn_enabled: true,
            ht_caps: crate::HtCapabilities {
                caps: crate::HTCAP_SGI20 | crate::HTCAP_SGI40,
                ..Default::default()
            },
            supported_ht_mcs: [1; 10],
            vht_caps: crate::VhtCapabilities {
                caps: (1 << 5) | (1 << 6),
                rx_mcs: 0,
                ..Default::default()
            },
            he_caps: crate::HeCapabilities {
                rx_mcs_80: 0,
                ..Default::default()
            },
        };
        let peer = PeerPhyConfig {
            ht_caps: crate::HtCapabilities {
                flags: crate::NODE_HTCAP,
                rx_mcs: [1; 10],
                caps: crate::HTCAP_SGI20 | crate::HTCAP_SGI40,
                ..Default::default()
            },
            ht_operation: crate::HtOperation {
                basic_mcs: [1; 16],
                ..Default::default()
            },
            vht_caps: crate::VhtCapabilities {
                flags: crate::NODE_VHTCAP,
                caps: (1 << 5) | (1 << 6),
                rx_mcs: 0,
                ..Default::default()
            },
            vht_operation: crate::VhtOperation {
                basic_mcs: 0,
                ..Default::default()
            },
            he_caps: crate::HeCapabilities {
                flags: crate::NODE_HECAP,
                rx_mcs_80: 0,
                basic_mcs: 0,
                ..Default::default()
            },
        };
        (local, peer)
    }

    #[test]
    fn try_another_bss_marks_current_failed_and_resets_all_band_mode() {
        let mut current = crate::AccessPoint {
            bssid: [1; 6],
            ssid: [0; 32],
            ssid_len: 4,
            channel: 6,
            is_2ghz: true,
            capability_info: crate::CAPINFO_ESS,
            rssi: 90,
            ..Default::default()
        };
        current.ssid[..4].copy_from_slice(b"home");
        let mut candidate = crate::AccessPoint {
            bssid: [2; 6],
            ssid: [0; 32],
            ssid_len: 4,
            channel: 36,
            is_5ghz: true,
            capability_info: crate::CAPINFO_ESS,
            rssi: 60,
            ..Default::default()
        };
        candidate.ssid[..4].copy_from_slice(b"home");
        let mut candidates = [current, candidate];
        let mut policy = crate::BssMatchPolicy {
            active_channels: &[6, 36],
            background_scan_active: false,
            background_scan: false,
            desired_channel: None,
            ibss_mode: false,
            privacy_enabled: false,
            desired_ssid: b"home",
            desired_bssid: None,
            rsn_enabled: false,
            psk_configured: false,
            local_rsn_protocols: crate::PROTO_RSN,
            local_rsn_akms: crate::AKM_PSK,
            local_rsn_ciphers: crate::CIPHER_CCMP,
            local_mfp_capable: false,
            local_mfp_required: false,
        };
        let mut mode = crate::PhyMode::G;
        let alternative = try_another_bss(
            &mut candidates,
            [1; 6],
            &policy,
            true,
            &mut mode,
            100,
            |_| 0,
        )
        .unwrap();
        assert_eq!(alternative.selected_index, 1);
        assert_eq!(alternative.phy_mode, crate::PhyMode::Auto);
        assert_eq!(candidates[0].previous_failures, 2); // current node is marked then skipped by chooser.
        policy.active_channels = &[6];
        let none = try_another_bss(
            &mut candidates[..1],
            [1; 6],
            &policy,
            false,
            &mut mode,
            100,
            |_| 0,
        );
        assert!(none.is_none());
    }

    #[test]
    fn phy_negotiation_gates_basic_mcs_crypto_and_channel_modes() {
        let (local, peer) = phy_config();
        assert_eq!(
            ht_negotiate(&local, &peer, crate::CIPHER_CCMP),
            (true, true, true)
        );
        assert_eq!(vht_negotiate(&local, &peer), (true, true, true));
        assert!(he_negotiate(&local, &peer, true));
        let mut no_basic = local;
        no_basic.supported_ht_mcs = [0; 10];
        assert_eq!(
            ht_negotiate(&no_basic, &peer, crate::CIPHER_CCMP),
            (false, false, false)
        );
        let mut tkip_local = local;
        tkip_local.rsn_enabled = true;
        assert_eq!(
            ht_negotiate(&tkip_local, &peer, crate::CIPHER_TKIP),
            (false, false, false)
        );
        let mut no_vht = local;
        no_vht.channel_supports_ac = false;
        assert_eq!(vht_negotiate(&no_vht, &peer), (false, false, false));
        assert!(!he_negotiate(&local, &peer, false));
    }

    #[test]
    fn failed_authentication_retries_bss_or_counts_peer_failure() {
        let mut state = OpenAuthState {
            authentication_state: true,
            rsn_enabled: false,
            is_bss_node: true,
            sequence: 2,
            status: 1,
            auth_subtype: 0xb0,
            node_failures: 0,
            bad_auth_count: 0,
            auth_fail_count: 0,
        };
        let effects = auth_open_station(&mut state);
        assert!(effects.try_another_bss);
        assert_eq!(state.auth_fail_count, 1);
        state.is_bss_node = false;
        let effects = auth_open_station(&mut state);
        assert!(!effects.try_another_bss);
        assert_eq!(state.node_failures, 1);
        assert_eq!(state.auth_fail_count, 2);
    }

    #[test]
    fn beacon_miss_threshold_tracks_interval_and_keeps_zero_interval() {
        assert_eq!(beacon_miss_threshold(100, 7), 30);
        assert_eq!(beacon_miss_threshold(200, 7), 15);
        assert_eq!(beacon_miss_threshold(0, 7), 7);
        assert_eq!(beacon_miss_threshold(1, 7), 30);
    }

    #[test]
    fn erp_reset_and_slot_changes_follow_band_and_capability_rules() {
        let state = reset_erp(crate::PhyMode::A, false, true, false, 0);
        assert_eq!(
            state,
            ErpState {
                use_protection: false,
                short_preamble: true,
                short_slot: true
            }
        );
        let state = reset_erp(crate::PhyMode::G, true, false, true, CAP_SHORT_SLOT);
        assert_eq!(
            state,
            ErpState {
                use_protection: false,
                short_preamble: false,
                short_slot: true
            }
        );
        let state = reset_erp(
            crate::PhyMode::G,
            true,
            false,
            false,
            CAP_SHORT_SLOT | CAP_SHORT_PREAMBLE,
        );
        assert_eq!(
            state,
            ErpState {
                use_protection: false,
                short_preamble: true,
                short_slot: false
            }
        );
        let mut flags = FLAG_USE_PROTECTION;
        assert!(set_short_slot(&mut flags, true));
        assert_eq!(flags, FLAG_USE_PROTECTION | FLAG_SHORT_SLOT);
        assert!(!set_short_slot(&mut flags, true));
    }

    #[test]
    fn negotiation_sorts_overwrites_basic_bits_and_deletes_unsupported() {
        let mut peer = RateSet::new(&[10, 0x8c, 18, 0x82]);
        let result = fix_rate(
            &mut peer,
            FixRateConfig {
                supported_rates: &local(),
                fixed_rate: None,
                hostap_mode: false,
            },
            FIX_RATE_SORT | FIX_RATE_NEGOTIATE | FIX_RATE_DELETE,
        );
        assert_eq!(result, 18);
        assert_eq!(peer.count, 3);
        assert_eq!(&peer.rates[..peer.count], &[0x82, 0x8c, 18]);
    }

    #[test]
    fn unsupported_basic_hostap_and_fixed_rate_fail_closed() {
        let mut peer = RateSet::new(&[0x80 | 10, 12]);
        assert_eq!(
            fix_rate(
                &mut peer,
                FixRateConfig {
                    supported_rates: &local(),
                    fixed_rate: None,
                    hostap_mode: true
                },
                FIX_RATE_NEGOTIATE
            ),
            12 | crate::LEGACY_RATE_BASIC
        );
        let mut station = RateSet::new(&[12]);
        let result = fix_rate(
            &mut station,
            FixRateConfig {
                supported_rates: &local(),
                fixed_rate: Some(1),
                hostap_mode: false,
            },
            FIX_RATE_FIXED | FIX_RATE_NEGOTIATE,
        );
        assert_eq!(result, 12 | crate::LEGACY_RATE_BASIC);
    }
}
