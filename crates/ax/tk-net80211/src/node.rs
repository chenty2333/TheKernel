//! ESS lookup, candidate validation and AP scoring from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_node.c` rev 1.217 and
//! `ieee80211_node.h` rev 1.64 (BSD-3-Clause). Copyright (c) 2001 Atsushi
//! Onoe, (c) 2002, 2003 Sam Leffler, Errno Consulting, and (c) 2008 Damien
//! Bergamini.

pub const PROTO_RSN: u32 = 1 << 0;
pub const PROTO_WPA: u32 = 1 << 1;
pub const PRIVACY: u16 = 0x0010;
pub const ESS_WEP_ON: u32 = 0x0000_0100;
pub const ESS_RSN_ON: u32 = 0x0020_0000;
pub const ESS_PSK: u32 = 0x0040_0000;
pub const ASSOCFAIL_PRIVACY: u32 = 0x04;
pub const ASSOCFAIL_ESSID: u32 = 0x10;
pub const ASSOCFAIL_WPA_PROTO: u32 = 0x40;
pub const FLAG_AUTO_JOIN: u32 = 0x1000_0000;
pub const HTOP0_SCO_MASK: u8 = 0x03;
pub const HTOP0_SCO_SHIFT: u8 = 0;
pub const HTOP0_SCO_SCN: u8 = 0;
pub const HTOP0_SCO_SCA: u8 = 1;
pub const HTOP0_SCO_SCB: u8 = 3;

const VALID_SECONDARY_ABOVE: [u8; 21] = [
    5, 6, 7, 8, 9, 10, 11, 12, 13, 40, 48, 56, 64, 104, 112, 120, 128, 136, 144, 153, 161,
];
const VALID_SECONDARY_BELOW: [u8; 21] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 36, 44, 52, 60, 100, 108, 116, 124, 132, 140, 149, 157,
];
const VALID_80MHZ_CENTERS: [u8; 8] = [42, 50, 58, 106, 112, 114, 138, 155];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetworkProfile<'a> {
    /// Empty SSID means join-any, as in OpenBSD's ESS list.
    pub ssid: &'a [u8],
    pub flags: u32,
    pub rsn_protocols: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccessPoint {
    pub ssid: [u8; 32],
    pub ssid_len: usize,
    pub bssid: [u8; 6],
    pub rssi: u8,
    pub is_2ghz: bool,
    pub is_5ghz: bool,
    pub rsn_protocols: u32,
    pub capability_info: u16,
    pub supports_ht: bool,
    pub supports_vht: bool,
    pub previous_failures: u8,
    pub association_failures: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EssSelection {
    pub access_point: usize,
    pub profile: usize,
}

impl AccessPoint {
    pub fn ssid(&self) -> &[u8] {
        &self.ssid[..self.ssid_len.min(self.ssid.len())]
    }
}

/// Search the configured ESS list by SSID.
// upstream: ieee80211_node.c ieee80211_get_ess()
pub fn get_ess<'a>(
    profiles: &'a [NetworkProfile<'a>],
    ssid: &[u8],
) -> Option<&'a NetworkProfile<'a>> {
    profiles
        .iter()
        .find(|profile| profile.ssid.len() == ssid.len() && profile.ssid == ssid)
}

/// Check SSID/security compatibility and record the source failure reason.
// upstream: ieee80211_node.c ieee80211_match_ess()
pub fn match_ess(profile: &NetworkProfile<'_>, ap: &mut AccessPoint) -> bool {
    if !profile.ssid.is_empty() && profile.ssid != ap.ssid() {
        ap.association_failures |= ASSOCFAIL_ESSID;
        return false;
    }
    if profile.flags & (ESS_PSK | ESS_RSN_ON) != 0 {
        if ap.rsn_protocols & PROTO_RSN != 0 && profile.rsn_protocols & PROTO_RSN == 0 {
            ap.association_failures |= ASSOCFAIL_WPA_PROTO;
            return false;
        }
        if ap.rsn_protocols & PROTO_WPA != 0 && profile.rsn_protocols & PROTO_WPA == 0 {
            ap.association_failures |= ASSOCFAIL_WPA_PROTO;
            return false;
        }
    } else if profile.flags & ESS_WEP_ON != 0 {
        if ap.capability_info & PRIVACY == 0 {
            ap.association_failures |= ASSOCFAIL_PRIVACY;
            return false;
        }
    } else if ap.capability_info & PRIVACY != 0 {
        ap.association_failures |= ASSOCFAIL_PRIVACY;
        return false;
    }
    if profile.ssid.is_empty() && ap.capability_info & PRIVACY != 0 {
        ap.association_failures |= ASSOCFAIL_PRIVACY;
        return false;
    }
    true
}

/// Slightly penalize 2.4-GHz RSSI before AP comparison.
// upstream: ieee80211_node.c ieee80211_ess_adjust_rssi()
pub fn ess_adjust_rssi(ap: &AccessPoint, max_rssi: u8) -> u8 {
    let mut rssi = ap.rssi;
    if ap.is_2ghz {
        if max_rssi != 0 {
            let penalty = (5 * max_rssi) / 100;
            if rssi >= penalty {
                rssi -= penalty;
            }
        } else if rssi >= 8 {
            rssi -= 8;
        }
    }
    rssi
}

/// Score saved ESS, WPA/RSN, privacy, 5-GHz signal, HT/VHT and prior failures.
// upstream: ieee80211_node.c ieee80211_ess_calculate_score()
pub fn ess_calculate_score(ap: &AccessPoint, has_profile: bool, max_rssi: u8) -> i32 {
    let mut score = 0;
    let min_5ghz_rssi = if max_rssi != 0 { 50 } else { 70 };
    if has_profile {
        score += 32;
    }
    if ap.rsn_protocols & PROTO_RSN != 0 {
        score += 16;
    }
    if ap.rsn_protocols & PROTO_WPA != 0 {
        score += 8;
    }
    if ap.capability_info & PRIVACY != 0 {
        score += 4;
    }
    if ap.is_5ghz && ap.rssi > min_5ghz_rssi {
        score += 2;
    }
    if ap.supports_ht {
        score += 1;
    }
    if ap.supports_vht {
        score += 1;
    }
    if ap.previous_failures == 0 {
        score += 21;
    }
    score
}

/// Prefer a candidate only if its source score (with adjusted RSSI tie-break) is greater.
// upstream: ieee80211_node.c ieee80211_ess_is_better()
pub fn ess_is_better(
    current: &AccessPoint,
    candidate: &AccessPoint,
    current_has_profile: bool,
    candidate_has_profile: bool,
    max_rssi: u8,
) -> bool {
    let current_score = ess_calculate_score(current, current_has_profile, max_rssi);
    let mut candidate_score = ess_calculate_score(candidate, candidate_has_profile, max_rssi);
    if ess_adjust_rssi(candidate, max_rssi) > ess_adjust_rssi(current, max_rssi) {
        candidate_score += 1;
    }
    candidate_score > current_score
}

/// Find a compatible ESS/AP candidate, honoring fixed selection and auto-join.
// upstream: ieee80211_node.c ieee80211_switch_ess()
pub fn switch_ess(
    profiles: &[NetworkProfile<'_>],
    access_points: &mut [AccessPoint],
    desired_ssid: &[u8],
    desired_bssid: Option<[u8; 6]>,
    flags: u32,
    interface_running: bool,
    max_rssi: u8,
) -> Option<EssSelection> {
    if !interface_running {
        return None;
    }
    let auto_join = flags & FLAG_AUTO_JOIN != 0;
    let mut selected: Option<EssSelection> = None;
    let mut selected_ap: Option<AccessPoint> = None;
    for (ap_index, ap) in access_points.iter_mut().enumerate() {
        if desired_bssid.is_some_and(|bssid| ap.bssid != bssid) {
            continue;
        }
        let Some(profile_index) = profiles.iter().position(|profile| match_ess(profile, ap)) else {
            continue;
        };
        if !auto_join {
            if desired_ssid == ap.ssid() {
                return Some(EssSelection {
                    access_point: ap_index,
                    profile: profile_index,
                });
            }
            continue;
        }
        if selected.is_none() {
            selected = Some(EssSelection {
                access_point: ap_index,
                profile: profile_index,
            });
            selected_ap = Some(*ap);
            continue;
        };
        if ess_is_better(
            selected_ap.as_ref().expect("selected AP snapshot"),
            ap,
            true,
            true,
            max_rssi,
        ) {
            selected = Some(EssSelection {
                access_point: ap_index,
                profile: profile_index,
            });
            selected_ap = Some(*ap);
        }
    }
    let selected = selected?;
    if access_points[selected.access_point].ssid() == desired_ssid {
        None
    } else {
        Some(selected)
    }
}

// upstream: ieee80211_node.c ieee80211_40mhz_valid_secondary_above()
pub fn valid_40mhz_secondary_above(primary_channel: u8) -> bool {
    if !((1..=9).contains(&primary_channel) || (36..=157).contains(&primary_channel)) {
        return false;
    }
    VALID_SECONDARY_ABOVE.contains(&primary_channel.wrapping_add(4))
}

// upstream: ieee80211_node.c ieee80211_40mhz_valid_secondary_below()
pub fn valid_40mhz_secondary_below(primary_channel: u8) -> bool {
    if !((5..=13).contains(&primary_channel) || (40..=161).contains(&primary_channel)) {
        return false;
    }
    VALID_SECONDARY_BELOW.contains(&primary_channel.wrapping_sub(4))
}

// upstream: ieee80211_node.c ieee80211_40mhz_center_freq_valid()
pub fn valid_40mhz_center_frequency(primary_channel: u8, htop0: u8) -> bool {
    match (htop0 & HTOP0_SCO_MASK) >> HTOP0_SCO_SHIFT {
        HTOP0_SCO_SCN => true,
        HTOP0_SCO_SCA => valid_40mhz_secondary_above(primary_channel),
        HTOP0_SCO_SCB => valid_40mhz_secondary_below(primary_channel),
        _ => false,
    }
}

// upstream: ieee80211_node.c ieee80211_80mhz_center_freq_valid()
pub fn valid_80mhz_center_frequency(channel_index: u8) -> bool {
    VALID_80MHZ_CENTERS.contains(&channel_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ap(ssid: &[u8], band: bool, rssi: u8) -> AccessPoint {
        let mut node = AccessPoint {
            is_2ghz: !band,
            is_5ghz: band,
            rssi,
            ..Default::default()
        };
        node.ssid[..ssid.len()].copy_from_slice(ssid);
        node.ssid_len = ssid.len();
        node
    }

    #[test]
    fn regulatory_ht_vht_center_channel_tables_match_openbsd() {
        assert!(valid_40mhz_secondary_above(1));
        assert!(!valid_40mhz_secondary_above(10));
        assert!(valid_40mhz_secondary_below(9));
        assert!(!valid_40mhz_secondary_below(14));
        assert!(valid_40mhz_center_frequency(36, HTOP0_SCO_SCA));
        assert!(!valid_40mhz_center_frequency(10, HTOP0_SCO_SCA));
        assert!(valid_40mhz_center_frequency(10, HTOP0_SCO_SCN));
        assert!(valid_80mhz_center_frequency(42));
        assert!(!valid_80mhz_center_frequency(43));
    }

    #[test]
    fn ess_lookup_and_protocol_privacy_checks_preserve_failure_reasons() {
        let profile = NetworkProfile {
            ssid: b"office",
            flags: ESS_RSN_ON | ESS_PSK,
            rsn_protocols: PROTO_RSN,
        };
        assert_eq!(get_ess(&[profile], b"office"), Some(&profile));
        let mut node = ap(b"office", true, 70);
        node.rsn_protocols = PROTO_WPA;
        assert!(!match_ess(&profile, &mut node));
        assert_eq!(node.association_failures, ASSOCFAIL_WPA_PROTO);
        let open = NetworkProfile {
            ssid: b"office",
            flags: 0,
            rsn_protocols: 0,
        };
        node.association_failures = 0;
        node.capability_info = PRIVACY;
        assert!(!match_ess(&open, &mut node));
        assert_eq!(node.association_failures, ASSOCFAIL_PRIVACY);
    }

    #[test]
    fn auto_join_selects_best_saved_network_and_fixed_join_honors_target() {
        let profiles = [NetworkProfile {
            ssid: b"home",
            flags: 0,
            rsn_protocols: 0,
        }];
        let mut candidates = [
            ap(b"home", false, 60),
            ap(b"home", true, 80),
            ap(b"other", true, 90),
        ];
        candidates[0].bssid = [1; 6];
        candidates[1].bssid = [2; 6];
        candidates[2].bssid = [3; 6];
        assert_eq!(
            switch_ess(
                &profiles,
                &mut candidates,
                b"old",
                None,
                FLAG_AUTO_JOIN,
                true,
                0
            ),
            Some(EssSelection {
                access_point: 1,
                profile: 0
            })
        );
        assert_eq!(
            switch_ess(
                &profiles,
                &mut candidates,
                b"home",
                Some([1; 6]),
                0,
                true,
                0
            ),
            Some(EssSelection {
                access_point: 0,
                profile: 0
            })
        );
        assert_eq!(
            switch_ess(
                &profiles,
                &mut candidates,
                b"home",
                None,
                FLAG_AUTO_JOIN,
                false,
                0
            ),
            None
        );
    }

    #[test]
    fn candidate_score_and_two_ghz_rssi_penalty_follow_openbsd_weights() {
        let mut current = ap(b"mesh", false, 72);
        current.rsn_protocols = PROTO_RSN;
        current.capability_info = PRIVACY;
        current.supports_ht = true;
        let mut candidate = ap(b"mesh", true, 68);
        candidate.rsn_protocols = PROTO_RSN;
        candidate.capability_info = PRIVACY;
        candidate.supports_ht = true;
        candidate.supports_vht = true;
        assert_eq!(ess_adjust_rssi(&current, 0), 64);
        assert_eq!(ess_adjust_rssi(&candidate, 0), 68);
        assert!(ess_is_better(&current, &candidate, true, true, 0));
        current.previous_failures = 1;
        assert_eq!(ess_calculate_score(&current, true, 0), 32 + 16 + 4 + 1);
    }
}
