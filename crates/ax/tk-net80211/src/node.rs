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
pub const ASSOCFAIL_CHAN: u32 = 0x01;
pub const ASSOCFAIL_IBSS: u32 = 0x02;
pub const ASSOCFAIL_BASIC_RATE: u32 = 0x08;
pub const ASSOCFAIL_BSSID: u32 = 0x20;
pub const ASSOCFAIL_WPA_KEY: u32 = 0x80;
pub const ASSOCFAIL_CSA: u32 = 0x100;
pub const CAPINFO_ESS: u16 = 0x0001;
pub const CAPINFO_IBSS: u16 = 0x0002;
pub const FLAG_AUTO_JOIN: u32 = 0x1000_0000;
pub const RSN_CAP_MFPC: u16 = 0x0080;
pub const LOCAL_CAP_MFP: u32 = 0x0000_2000;
pub const AKM_8021X: u32 = 0x01;
pub const AKM_PSK: u32 = 0x02;
pub const AKM_SHA256_8021X: u32 = 0x04;
pub const AKM_SHA256_PSK: u32 = 0x08;
pub const CIPHER_TKIP: u32 = 0x04;
pub const CIPHER_CCMP: u32 = 0x08;
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
    pub channel: u8,
    pub rates: crate::RateSet,
    pub group_cipher: u32,
    pub rsn_ciphers: u32,
    pub rsn_akms: u32,
    pub group_management_cipher: u32,
    pub rsn_capabilities: u16,
    pub channel_switch_announcement: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EssSelection {
    pub access_point: usize,
    pub profile: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalRsnPolicy {
    pub protocols: u32,
    pub akms: u32,
    pub ciphers: u32,
    pub flags: u32,
    pub capabilities: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RsnChoice {
    pub protocol: u32,
    pub akm: u32,
    pub cipher: u32,
    pub pmkid: Option<[u8; 16]>,
    pub mfp: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BssMatchPolicy<'a> {
    pub active_channels: &'a [u8],
    pub background_scan_active: bool,
    pub background_scan: bool,
    pub desired_channel: Option<u8>,
    pub ibss_mode: bool,
    pub privacy_enabled: bool,
    pub desired_ssid: &'a [u8],
    pub desired_bssid: Option<[u8; 6]>,
    pub rsn_enabled: bool,
    pub psk_configured: bool,
    pub local_rsn_protocols: u32,
    pub local_rsn_akms: u32,
    pub local_rsn_ciphers: u32,
    pub local_mfp_capable: bool,
    pub local_mfp_required: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BssSelection {
    pub selected: Option<usize>,
    pub current: Option<usize>,
    pub evicted: alloc::vec::Vec<usize>,
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

/// Prefer RSN/SHA-256/CCMP while intersecting local and peer capabilities.
// upstream: ieee80211_node.c ieee80211_choose_rsnparams()
pub fn choose_rsn_params(
    peer_protocols: u32,
    peer_akms: u32,
    peer_ciphers: u32,
    peer_capabilities: u16,
    local: LocalRsnPolicy,
    cached_pmkid: Option<[u8; 16]>,
) -> RsnChoice {
    let protocols = peer_protocols & local.protocols;
    let protocol = if protocols & PROTO_RSN != 0 {
        PROTO_RSN
    } else {
        PROTO_WPA
    };
    let akms = peer_akms & local.akms;
    let akm = if local.flags & ESS_PSK != 0 && akms & (AKM_PSK | AKM_SHA256_PSK) != 0 {
        if akms & AKM_SHA256_PSK != 0 {
            AKM_SHA256_PSK
        } else {
            AKM_PSK
        }
    } else if akms & AKM_SHA256_8021X != 0 {
        AKM_SHA256_8021X
    } else {
        AKM_8021X
    };
    let ciphers = peer_ciphers & local.ciphers;
    let cipher = if ciphers & CIPHER_CCMP != 0 {
        CIPHER_CCMP
    } else {
        CIPHER_TKIP
    };
    RsnChoice {
        protocol,
        akm,
        cipher,
        pmkid: if protocol == PROTO_RSN && akm & (AKM_8021X | AKM_SHA256_8021X) != 0 {
            cached_pmkid
        } else {
            None
        },
        mfp: local.capabilities & LOCAL_CAP_MFP != 0 && peer_capabilities & RSN_CAP_MFPC != 0,
    }
}

/// Return the fixed or negotiated legacy rate without its Basic flag.
// upstream: ieee80211_node.c ieee80211_get_rate()
pub fn get_rate(
    rates: &crate::RateSet,
    fixed_rate: Option<usize>,
    interface_running: bool,
    negotiated_tx_rate: usize,
) -> u8 {
    let index = fixed_rate.or_else(|| interface_running.then_some(negotiated_tx_rate));
    index
        .and_then(|index| rates.rates.get(index).copied())
        .unwrap_or(0)
        & crate::LEGACY_RATE_VALUE
}

// upstream: ieee80211_node.c ieee80211_node_getrssi()
pub fn get_rssi(ap: &AccessPoint) -> u8 {
    ap.rssi
}

/// Check roaming RSSI using calibrated percentage thresholds or raw defaults.
// upstream: ieee80211_node.c ieee80211_node_checkrssi()
pub fn check_rssi(ap: &AccessPoint, channel_valid: bool, max_rssi: u8) -> bool {
    if !channel_valid {
        return false;
    }
    if max_rssi != 0 {
        let threshold = if ap.is_2ghz { 60 } else { 50 };
        u16::from(ap.rssi) * 100 / u16::from(max_rssi) >= threshold
    } else {
        let threshold = if ap.is_2ghz {
            (-60i8) as u8
        } else {
            (-70i8) as u8
        };
        ap.rssi >= threshold
    }
}

/// Validate a scanned BSS against channel, mode, rates, SSID, BSSID and RSN policy.
// upstream: ieee80211_node.c ieee80211_match_bss()
pub fn match_bss(
    policy: &BssMatchPolicy<'_>,
    ap: &mut AccessPoint,
    current_bss_failure: &mut u32,
    mut fix_rate: impl FnMut(&mut crate::RateSet) -> u8,
) -> u32 {
    let mut fail = 0;
    if !policy.background_scan_active && !policy.active_channels.contains(&ap.channel) {
        fail |= ASSOCFAIL_CHAN;
    }
    if policy
        .desired_channel
        .is_some_and(|channel| channel != ap.channel)
    {
        fail |= ASSOCFAIL_CHAN;
    }
    if policy.ibss_mode {
        if ap.capability_info & CAPINFO_IBSS == 0 {
            fail |= ASSOCFAIL_IBSS;
        }
    } else if ap.capability_info & CAPINFO_ESS == 0 {
        fail |= ASSOCFAIL_IBSS;
    }
    if policy.privacy_enabled {
        if ap.capability_info & PRIVACY == 0 {
            fail |= ASSOCFAIL_PRIVACY;
        }
    } else if ap.capability_info & PRIVACY != 0 {
        fail |= ASSOCFAIL_PRIVACY;
    }
    let rate = fix_rate(&mut ap.rates);
    if rate & crate::LEGACY_RATE_BASIC != 0 {
        fail |= ASSOCFAIL_BASIC_RATE;
    }
    if policy.desired_ssid.is_empty() {
        fail |= ASSOCFAIL_ESSID;
    } else if policy.desired_ssid != ap.ssid() {
        fail |= ASSOCFAIL_ESSID;
    }
    if policy.desired_bssid.is_some_and(|bssid| bssid != ap.bssid) {
        fail |= ASSOCFAIL_BSSID;
    }
    if ap.channel_switch_announcement {
        fail |= ASSOCFAIL_CSA;
    }
    if policy.rsn_enabled {
        if ap.rsn_protocols & policy.local_rsn_protocols == 0
            || ap.rsn_akms & policy.local_rsn_akms == 0
            || (ap.rsn_akms & policy.local_rsn_akms & !(AKM_PSK | AKM_SHA256_PSK) == 0
                && !policy.psk_configured)
            || !matches!(ap.group_cipher, 2 | CIPHER_TKIP | CIPHER_CCMP | 16)
            || ap.rsn_ciphers & policy.local_rsn_ciphers == 0
            || (ap.rsn_capabilities & RSN_CAP_MFPC != 0 && ap.group_management_cipher != 0x20)
            || (!policy.local_mfp_capable && ap.rsn_capabilities & 0x0040 != 0)
            || (policy.local_mfp_capable
                && policy.local_mfp_required
                && ap.rsn_capabilities & RSN_CAP_MFPC == 0)
        {
            fail |= ASSOCFAIL_WPA_PROTO;
        }
    }
    if policy.background_scan {
        if fail & ASSOCFAIL_ESSID == 0 {
            ap.association_failures = fail;
        }
    } else {
        ap.association_failures = fail;
    }
    if fail & ASSOCFAIL_ESSID == 0 {
        *current_bss_failure = ap.association_failures;
    }
    fail
}

/// Select the strongest eligible BSS, retaining OpenBSD's all-band 5-GHz preference.
// upstream: ieee80211_node.c ieee80211_node_choose_bss()
pub fn choose_bss(
    policy: &BssMatchPolicy<'_>,
    access_points: &mut [AccessPoint],
    current_bssid: Option<[u8; 6]>,
    scan_all_bands: bool,
    max_rssi: u8,
    mut fix_rate: impl FnMut(&mut crate::RateSet) -> u8,
) -> BssSelection {
    let mut current = None;
    let mut best_any: Option<(usize, u8)> = None;
    let mut best_2ghz: Option<(usize, u8)> = None;
    let mut best_5ghz: Option<(usize, u8)> = None;
    let mut evicted = alloc::vec::Vec::new();
    for (index, ap) in access_points.iter_mut().enumerate() {
        if ap.previous_failures != 0 {
            let failures = ap.previous_failures;
            ap.previous_failures = failures.saturating_add(1);
            if failures > 2 {
                evicted.push(index);
            }
            continue;
        }
        if current_bssid == Some(ap.bssid) {
            current = Some(index);
        }
        let mut ignored_current_failure = 0;
        if match_bss(policy, ap, &mut ignored_current_failure, &mut fix_rate) != 0 {
            continue;
        }
        if scan_all_bands {
            if ap.is_2ghz && best_2ghz.is_none_or(|(_, rssi)| ap.rssi > rssi) {
                best_2ghz = Some((index, ap.rssi));
            } else if ap.is_5ghz && best_5ghz.is_none_or(|(_, rssi)| ap.rssi > rssi) {
                best_5ghz = Some((index, ap.rssi));
            }
        } else if best_any.is_none_or(|(_, rssi)| ap.rssi > rssi) {
            best_any = Some((index, ap.rssi));
        }
    }
    let selected = if scan_all_bands {
        if let Some((best_5, _)) =
            best_5ghz.filter(|(index, _)| check_rssi(&access_points[*index], true, max_rssi))
        {
            Some(best_5)
        } else if let (Some((best_5, rssi_5)), Some((best_2, rssi_2))) = (best_5ghz, best_2ghz) {
            Some(if rssi_5 >= rssi_2 { best_5 } else { best_2 })
        } else {
            best_2ghz
                .map(|(index, _)| index)
                .or_else(|| best_5ghz.map(|(index, _)| index))
        }
    } else {
        best_any.map(|(index, _)| index)
    };
    BssSelection {
        selected,
        current,
        evicted,
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

    fn match_policy<'a>(ssid: &'a [u8]) -> BssMatchPolicy<'a> {
        BssMatchPolicy {
            active_channels: &[1, 6, 11],
            background_scan_active: false,
            background_scan: false,
            desired_channel: None,
            ibss_mode: false,
            privacy_enabled: false,
            desired_ssid: ssid,
            desired_bssid: None,
            rsn_enabled: false,
            psk_configured: false,
            local_rsn_protocols: PROTO_RSN,
            local_rsn_akms: AKM_PSK,
            local_rsn_ciphers: CIPHER_CCMP,
            local_mfp_capable: false,
            local_mfp_required: false,
        }
    }

    #[test]
    fn bss_match_accumulates_failure_reasons_and_preserves_background_scan_state() {
        let mut candidate = ap(b"home", true, 80);
        candidate.channel = 6;
        candidate.capability_info = CAPINFO_ESS;
        candidate.rates = crate::RateSet::new(&[12]);
        let mut current_failure = 0;
        assert_eq!(
            match_bss(
                &match_policy(b"home"),
                &mut candidate,
                &mut current_failure,
                |_| 0
            ),
            0
        );
        assert_eq!(current_failure, 0);
        let mut bad = ap(b"other", true, 70);
        bad.channel = 36;
        bad.capability_info = CAPINFO_ESS | PRIVACY;
        bad.rates = crate::RateSet::new(&[12]);
        bad.association_failures = ASSOCFAIL_BSSID;
        let mut policy = match_policy(b"home");
        policy.background_scan_active = false;
        policy.background_scan = true;
        current_failure = ASSOCFAIL_CHAN;
        let failures = match_bss(&policy, &mut bad, &mut current_failure, |_| {
            crate::LEGACY_RATE_BASIC
        });
        assert_ne!(failures & ASSOCFAIL_CHAN, 0);
        assert_ne!(failures & ASSOCFAIL_PRIVACY, 0);
        assert_ne!(failures & ASSOCFAIL_ESSID, 0);
        assert_ne!(failures & ASSOCFAIL_BASIC_RATE, 0);
        assert_eq!(bad.association_failures, ASSOCFAIL_BSSID);
        assert_eq!(current_failure, ASSOCFAIL_CHAN);
    }

    #[test]
    fn bss_chooser_prefers_good_five_ghz_and_tracks_evictions() {
        let mut two = ap(b"home", false, 90);
        two.channel = 6;
        two.capability_info = CAPINFO_ESS;
        let mut five = ap(b"home", true, 60);
        five.channel = 36;
        five.capability_info = CAPINFO_ESS;
        let mut stale = ap(b"stale", false, 80);
        stale.channel = 1;
        stale.capability_info = CAPINFO_ESS;
        stale.previous_failures = 3;
        let mut two = two;
        two.bssid = [1; 6];
        let mut five = five;
        five.bssid = [2; 6];
        let mut candidates = [two, five, stale];
        let mut policy = match_policy(b"home");
        policy.active_channels = &[1, 6, 11, 36];
        let selection = choose_bss(&policy, &mut candidates, Some([1; 6]), true, 100, |_| 0);
        assert_eq!(selection.selected, Some(1));
        assert_eq!(selection.current, Some(0));
        assert_eq!(selection.evicted, [2]);
        candidates[1].rssi = 40;
        let selection = choose_bss(&policy, &mut candidates[..2], None, true, 100, |_| 0);
        assert_eq!(selection.selected, Some(0));
    }

    #[test]
    fn bss_match_checks_rsn_intersection_and_mfp_requirements() {
        let mut candidate = ap(b"home", true, 80);
        candidate.channel = 6;
        candidate.capability_info = CAPINFO_ESS | PRIVACY;
        candidate.rsn_protocols = PROTO_RSN;
        candidate.rsn_akms = AKM_PSK;
        candidate.rsn_ciphers = CIPHER_CCMP;
        candidate.group_cipher = CIPHER_CCMP;
        candidate.rsn_capabilities = RSN_CAP_MFPC;
        candidate.group_management_cipher = 0x20;
        let mut policy = match_policy(b"home");
        policy.privacy_enabled = true;
        policy.rsn_enabled = true;
        policy.local_mfp_capable = true;
        policy.local_mfp_required = true;
        policy.psk_configured = true;
        let mut current = 0;
        assert_eq!(match_bss(&policy, &mut candidate, &mut current, |_| 0), 0);
        policy.local_mfp_required = true;
        candidate.rsn_capabilities = 0;
        assert_ne!(
            match_bss(&policy, &mut candidate, &mut current, |_| 0) & ASSOCFAIL_WPA_PROTO,
            0
        );
    }

    #[test]
    fn reported_rate_and_roaming_rssi_keep_source_defaults() {
        let rates = crate::RateSet::new(&[0x82, 0x8c, 24]);
        assert_eq!(get_rate(&rates, Some(1), false, 0), 12);
        assert_eq!(get_rate(&rates, None, true, 2), 24);
        assert_eq!(get_rate(&rates, None, false, 2), 0);
        let two = ap(b"two", false, 59);
        let five = ap(b"five", true, 50);
        assert_eq!(get_rssi(&two), 59);
        assert!(!check_rssi(&two, true, 100));
        assert!(check_rssi(&two, true, 98));
        assert!(check_rssi(&five, true, 100));
        assert!(check_rssi(&ap(b"raw", false, 200), true, 0));
        assert!(!check_rssi(&two, false, 100));
    }

    #[test]
    fn rsn_choice_prefers_stronger_suites_and_reuses_enterprise_pmkid() {
        let policy = LocalRsnPolicy {
            protocols: PROTO_RSN | PROTO_WPA,
            akms: AKM_8021X | AKM_PSK | AKM_SHA256_8021X | AKM_SHA256_PSK,
            ciphers: CIPHER_TKIP | CIPHER_CCMP,
            flags: ESS_PSK,
            capabilities: LOCAL_CAP_MFP,
        };
        let psk = choose_rsn_params(
            PROTO_RSN | PROTO_WPA,
            AKM_PSK | AKM_SHA256_PSK,
            CIPHER_TKIP | CIPHER_CCMP,
            RSN_CAP_MFPC,
            policy,
            None,
        );
        assert_eq!(psk.protocol, PROTO_RSN);
        assert_eq!(psk.akm, AKM_SHA256_PSK);
        assert_eq!(psk.cipher, CIPHER_CCMP);
        assert!(psk.mfp);
        assert_eq!(psk.pmkid, None);
        let pmkid = [0x5a; 16];
        let enterprise = choose_rsn_params(
            PROTO_RSN,
            AKM_8021X | AKM_SHA256_8021X,
            CIPHER_TKIP,
            RSN_CAP_MFPC,
            LocalRsnPolicy { flags: 0, ..policy },
            Some(pmkid),
        );
        assert_eq!(enterprise.akm, AKM_SHA256_8021X);
        assert_eq!(enterprise.pmkid, Some(pmkid));
        assert_eq!(enterprise.cipher, CIPHER_TKIP);
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
