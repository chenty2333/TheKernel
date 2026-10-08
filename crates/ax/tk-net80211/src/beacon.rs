//! Beacon and Probe Response processing from OpenBSD net80211.
//!
//! Station scan-path translation of `sys/net80211/ieee80211_input.c` rev 1.263,
//! `ieee80211_node.c` rev 1.217, `ieee80211.h` rev 1.137 and
//! `ieee80211_var.h` rev 1.157 (BSD-3-Clause). Copyright (c) 2001 Atsushi Onoe;
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting; Copyright (c)
//! 2007-2009 Damien Bergamini.

use crate::{
    FixRateConfig, NodeAllocError, NodeLifecycle, NodeTable, PeerRateState, RateIeError, RateSet,
    RsnParams, alloc_node, find_node, find_node_mut, fix_rate, parse_edca_ie, parse_rsn,
    parse_wmm_params, parse_wmm_qos_info, setup_he_caps, setup_he_operation, setup_ht_caps,
    setup_ht_operation, setup_rates, setup_vht_caps, setup_vht_operation,
};

const MAC_HEADER_LEN: usize = 24;
const FIXED_BEACON_LEN: usize = 12;
const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_MGT: u8 = 0x00;
const FC0_SUBTYPE_MASK: u8 = 0xf0;
const FC0_SUBTYPE_BEACON: u8 = 0x80;
const FC0_SUBTYPE_PROBE_RESP: u8 = 0x50;
const EID_SSID: u8 = 0;
const EID_RATES: u8 = 1;
const EID_DS_PARAMS: u8 = 3;
const EID_TIM: u8 = 5;
const EID_EDCA: u8 = 12;
const EID_XRATES: u8 = 50;
const EID_RSN: u8 = 48;
const EID_ERP: u8 = 42;
const EID_CSA: u8 = 37;
const EID_XCSA: u8 = 60;
const EID_HT_CAPS: u8 = 45;
const EID_HT_OP: u8 = 61;
const EID_VHT_CAPS: u8 = 191;
const EID_VHT_OP: u8 = 192;
const EID_EXTENSION: u8 = 255;
const EXT_HE_CAPS: u8 = 35;
const EXT_HE_OP: u8 = 36;
const EID_VENDOR: u8 = 221;
const WPA_OUI: [u8; 3] = [0x00, 0x50, 0xf2];
const WMM_AP_UAPSD: u8 = 0x80;
const ERP_USE_PROTECTION: u8 = 0x02;
const CAPINFO_SHORT_SLOTTIME: u16 = 0x0400;
const HTCAP_CBW20_40: u16 = 0x0002;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeaconError {
    ShortFrame,
    NotBeaconOrProbeResponse,
    InvalidSupportedRates,
    InvalidSsid,
    InvalidChannel,
    ChannelMismatch,
    NodeAllocation(NodeAllocError),
    RateIe(RateIeError),
    SavedIe(crate::SaveIeError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeaconRxInfo {
    pub receive_channel: Option<u8>,
    pub rssi: u8,
    pub timestamp: u64,
    pub is_probe_response: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeaconPolicy<'a> {
    pub active_channels: &'a [u8],
    pub scan_all: bool,
    pub background_scan: bool,
    pub state_scanning: bool,
    pub state_running: bool,
    pub current_channel: u8,
    pub current_mode: crate::PhyMode,
    pub local_ht_caps: u16,
    pub local_vht_caps: u32,
    pub local_vht_tx_max_lgi_mbps: u16,
    pub local_channel_160_allowed: bool,
    pub station_mode: bool,
    pub local_rsn_capable: bool,
    pub local_qos_enabled: bool,
    pub local_uapsd_enabled: bool,
    pub local_uapsd_access_categories: u8,
    pub local_uapsd_max_service_period: u8,
    pub local_rates: &'a RateSet,
    pub fixed_rate: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BeaconUpdate {
    pub is_new_node: bool,
    pub channel: u8,
    pub malformed_ie_count: u32,
    pub rates_status: u8,
    pub protection_changed: Option<bool>,
    pub ht_protection_changed: bool,
    pub forty_mhz_changed: bool,
    pub short_slot_changed: Option<bool>,
    pub dtim_changed: bool,
    pub reset_management_timer: bool,
}

#[derive(Default)]
struct BeaconIes<'a> {
    ssid: Option<&'a [u8]>,
    rates: Option<&'a [u8]>,
    extended_rates: Option<&'a [u8]>,
    channel: Option<u8>,
    erp: Option<u8>,
    csa: bool,
    edca: Option<&'a [u8]>,
    wmm: Option<&'a [u8]>,
    rsn: Option<&'a [u8]>,
    wpa: Option<&'a [u8]>,
    htcaps: Option<&'a [u8]>,
    htop: Option<&'a [u8]>,
    vhtcaps: Option<&'a [u8]>,
    vhtop: Option<&'a [u8]>,
    hecaps: Option<&'a [u8]>,
    heop: Option<&'a [u8]>,
    tim: Option<&'a [u8]>,
}

fn parse_beacon_ies(mut bytes: &[u8], initial_channel: u8) -> (BeaconIes<'_>, u32) {
    let mut ies = BeaconIes {
        channel: Some(initial_channel),
        ..Default::default()
    };
    let mut malformed = 0;
    while bytes.len() >= 2 {
        let len = usize::from(bytes[1]);
        let Some(end) = 2usize.checked_add(len).filter(|&end| end <= bytes.len()) else {
            malformed += 1;
            break;
        };
        let element = &bytes[..end];
        let payload = &element[2..];
        match element[0] {
            EID_SSID => ies.ssid = Some(payload),
            EID_RATES => ies.rates = Some(element),
            EID_XRATES => ies.extended_rates = Some(element),
            EID_DS_PARAMS => {
                if let Some(&channel) = payload.first() {
                    ies.channel = Some(channel);
                } else {
                    malformed += 1;
                }
            }
            EID_ERP => {
                if let Some(&erp) = payload.first() {
                    ies.erp = Some(erp);
                } else {
                    malformed += 1;
                }
            }
            EID_CSA => {
                if payload.len() >= 3 {
                    ies.csa = true;
                } else {
                    malformed += 1;
                }
            }
            EID_XCSA => {
                if payload.len() >= 4 {
                    ies.csa = true;
                } else {
                    malformed += 1;
                }
            }
            EID_RSN => ies.rsn = Some(element),
            EID_EDCA => {
                if payload.len() >= 18 {
                    ies.edca = Some(element);
                }
            }
            EID_HT_CAPS => ies.htcaps = Some(element),
            EID_HT_OP => {
                if payload.len() >= 22 {
                    ies.htop = Some(element);
                    ies.channel = Some(payload[0]);
                } else {
                    malformed += 1;
                }
            }
            EID_VHT_CAPS => ies.vhtcaps = Some(element),
            EID_VHT_OP => ies.vhtop = Some(element),
            EID_TIM => {
                if payload.len() >= 4 {
                    ies.tim = Some(element);
                } else {
                    malformed += 1;
                }
            }
            EID_EXTENSION => {
                if let Some(extension) = payload.first() {
                    match *extension {
                        EXT_HE_CAPS => ies.hecaps = Some(element),
                        EXT_HE_OP => ies.heop = Some(element),
                        _ => {}
                    }
                } else {
                    malformed += 1;
                }
            }
            EID_VENDOR => {
                if payload.len() < 4 {
                    malformed += 1;
                } else if payload[..3] == WPA_OUI {
                    if payload[3] == 1 {
                        ies.wpa = Some(element);
                    } else if payload.len() >= 5 && payload[3] == 2 && payload[4] == 1 {
                        ies.wmm = Some(element);
                    }
                }
            }
            _ => {}
        }
        bytes = &bytes[end..];
    }
    if !bytes.is_empty() && bytes.len() < 2 {
        malformed += 1;
    }
    (ies, malformed)
}

/// Process the station scan path of a received Beacon/Probe Response.
// upstream: ieee80211_input.c ieee80211_recv_probe_resp()
pub fn receive_beacon(
    table: &mut NodeTable,
    frame: &[u8],
    rx: BeaconRxInfo,
    policy: &BeaconPolicy<'_>,
    current_bssid: Option<[u8; 6]>,
) -> Result<BeaconUpdate, BeaconError> {
    if frame.len() < MAC_HEADER_LEN + FIXED_BEACON_LEN {
        return Err(BeaconError::ShortFrame);
    }
    if frame[0] & FC0_TYPE_MASK != FC0_TYPE_MGT
        || !matches!(
            frame[0] & FC0_SUBTYPE_MASK,
            FC0_SUBTYPE_BEACON | FC0_SUBTYPE_PROBE_RESP
        )
    {
        return Err(BeaconError::NotBeaconOrProbeResponse);
    }
    let source = frame[10..16].try_into().expect("six-byte source address");
    let bssid: [u8; 6] = frame[16..22].try_into().expect("six-byte BSSID");
    let timestamp: [u8; 8] = frame[24..32].try_into().expect("fixed beacon timestamp");
    let beacon_interval = u16::from_le_bytes([frame[32], frame[33]]);
    let capability = u16::from_le_bytes([frame[34], frame[35]]);
    let receive_channel = rx
        .receive_channel
        .filter(|&channel| channel != 0)
        .unwrap_or(policy.current_channel);
    let (ies, malformed_ie_count) = parse_beacon_ies(&frame[36..], receive_channel);
    let rates = ies.rates.ok_or(BeaconError::InvalidSupportedRates)?;
    if rates.len() < 2
        || usize::from(rates[1]) > crate::RATE_MAX_SIZE
        || usize::from(rates[1]) > rates.len() - 2
    {
        return Err(BeaconError::InvalidSupportedRates);
    }
    let ssid = ies.ssid.ok_or(BeaconError::InvalidSsid)?;
    if ssid.len() > 32 {
        return Err(BeaconError::InvalidSsid);
    }
    let channel = ies.channel.unwrap_or(receive_channel);
    if !policy.active_channels.contains(&channel) && !(policy.scan_all && policy.background_scan) {
        return Err(BeaconError::InvalidChannel);
    }
    if (rx
        .receive_channel
        .is_some_and(|rxch| rxch != 0 && channel != rxch))
        || ((!policy.state_scanning || !policy.scan_all) && channel != receive_channel)
    {
        return Err(BeaconError::ChannelMismatch);
    }
    let is_new_node = find_node(table, &source).is_none();
    if is_new_node {
        alloc_node(table, source).map_err(BeaconError::NodeAllocation)?;
    }
    let bss_before = table.bss_node.clone();
    let node = find_node_mut(table, &source).expect("allocated scan node");
    let old_rssi = node.access_point.rssi;
    let old_erp = node.erp;
    if let Some(ie) = ies.rsn {
        crate::save_information_element(&mut node.saved_rsn_ie, ie)
            .map_err(BeaconError::SavedIe)?;
    }
    if let Some(ie) = ies.wpa {
        crate::save_information_element(&mut node.saved_wpa_ie, ie)
            .map_err(BeaconError::SavedIe)?;
    }
    node.access_point.channel = channel;
    node.access_point.is_2ghz = channel <= 14;
    node.access_point.is_5ghz = channel > 14;
    node.access_point.capability_info = capability;
    node.access_point.channel_switch_announcement = ies.csa;
    if !ssid.is_empty() && node.access_point.ssid[0] == 0 {
        node.access_point.ssid.fill(0);
        node.access_point.ssid[..ssid.len()].copy_from_slice(ssid);
        node.access_point.ssid_len = ssid.len();
    }
    if let Some(htcaps) = ies.htcaps {
        let _ = setup_ht_caps(&mut node.ht_caps, &htcaps[2..]);
    }
    let valid_htop = ies
        .htop
        .is_some_and(|htop| setup_ht_operation(&mut node.ht_operation, &htop[2..], true));
    if ies.htcaps.is_some() && ies.vhtcaps.is_some() && node.access_point.is_5ghz {
        let _ = setup_vht_caps(&mut node.vht_caps, &ies.vhtcaps.unwrap()[2..]);
        if let Some(vhtop) = ies.vhtop {
            let _ = setup_vht_operation(
                &mut node.vht_operation,
                &node.ht_operation,
                node.vht_caps.caps,
                node.ht_operation.primary_channel,
                policy.local_channel_160_allowed,
                policy.local_vht_caps,
                policy.local_vht_tx_max_lgi_mbps,
                &vhtop[2..],
            );
        }
    }
    if let Some(hecaps) = ies.hecaps {
        if hecaps.len() >= 3 {
            let _ = setup_he_caps(&mut node.he_caps, &hecaps[3..]);
        }
    }
    if let Some(heop) = ies.heop {
        if heop.len() >= 3 {
            let _ = setup_he_operation(&mut node.he_caps, &heop[3..], true);
        }
    }
    if let Some(tim) = ies.tim {
        node.dtim_count = tim[2];
        node.dtim_period = tim[3];
    }
    let wmm_qos_info = ies.wmm.and_then(|ie| parse_wmm_qos_info(ie).ok());
    if policy.state_scanning || policy.background_scan {
        node.qos = ies.edca.is_some() || ies.wmm.is_some();
        node.supported_rsn_protocols = 0;
        node.supported_rsn_akms = 0;
        node.access_point.rsn_protocols = 0;
        node.access_point.rsn_akms = 0;
        node.access_point.rsn_ciphers = 0;
        node.access_point.group_cipher = 0;
        node.access_point.group_management_cipher = 0;
        node.access_point.rsn_capabilities = 0;
        if policy.local_rsn_capable {
            let rsn: Option<RsnParams> = ies.rsn.and_then(|ie| parse_rsn(ie).ok());
            let wpa: Option<RsnParams> = ies.wpa.and_then(|ie| crate::parse_wpa(ie).ok());
            if let Some(params) = &rsn {
                node.supported_rsn_protocols |= crate::PROTO_RSN;
                node.supported_rsn_akms |= params.akms;
            }
            if let Some(params) = &wpa {
                node.supported_rsn_protocols |= crate::PROTO_WPA;
                node.supported_rsn_akms |= params.akms;
            }
            let selected_protocol = if rsn.is_some() {
                crate::PROTO_RSN
            } else {
                crate::PROTO_WPA
            };
            if let Some(params) = rsn.or(wpa) {
                node.access_point.rsn_protocols = selected_protocol;
                node.access_point.rsn_akms = params.akms;
                node.access_point.rsn_ciphers = params.pairwise_ciphers;
                node.access_point.group_cipher = params.group_cipher as u32;
                node.access_point.group_management_cipher = params.group_management_cipher as u32;
                node.access_point.rsn_capabilities = params.capabilities;
            }
        }
    }
    if node.qos {
        let edca_ok = ies.edca.is_some_and(|edca| {
            parse_edca_ie(&mut node.edca, edca, policy.local_qos_enabled).is_ok()
        });
        let wmm_ok = !edca_ok
            && ies.wmm.is_some_and(|wmm| {
                parse_wmm_params(&mut node.edca, wmm, policy.local_qos_enabled).is_ok()
            });
        node.qos = edca_ok || wmm_ok;
    }
    node.uapsd = policy.local_uapsd_enabled
        && node.qos
        && wmm_qos_info.is_some_and(|info| info & WMM_AP_UAPSD != 0);
    node.access_point.bssid = bssid;
    if policy.state_scanning && node.access_point.is_5ghz {
        if rx.is_probe_response || old_rssi == 0 || old_rssi < rx.rssi {
            node.access_point.rssi = rx.rssi;
        }
    } else {
        node.access_point.rssi = rx.rssi;
    }
    node.beacon_timestamp = timestamp;
    node.receive_timestamp = rx.timestamp;
    node.beacon_interval = beacon_interval;
    node.erp = ies.erp.unwrap_or(0);
    let mut peer_rates = PeerRateState {
        rates: node.access_point.rates,
        ..Default::default()
    };
    let rate_ie_result = setup_rates(
        &mut peer_rates,
        rates,
        ies.extended_rates,
        node.access_point.is_2ghz,
        |rate_set| {
            i32::from(fix_rate(
                rate_set,
                FixRateConfig {
                    supported_rates: policy.local_rates,
                    fixed_rate: policy.fixed_rate,
                    hostap_mode: !policy.station_mode,
                },
                crate::FIX_RATE_SORT,
            ))
        },
    );
    let rates_status = rate_ie_result.map_err(BeaconError::RateIe)? as u8;
    node.access_point.rates = peer_rates.rates;
    node.access_point.supports_ht = crate::supports_ht(&node.ht_caps);
    node.access_point.supports_vht = crate::supports_vht(&node.vht_caps);
    node.lifecycle = NodeLifecycle::Cache;
    let new_erp = node.erp;
    let new_htop0 = node.ht_operation.htop0;
    let new_htop1 = node.ht_operation.htop1;
    let new_dtim_count = node.dtim_count;
    let new_dtim_period = node.dtim_period;

    let mut update = BeaconUpdate {
        is_new_node,
        channel,
        malformed_ie_count,
        rates_status,
        ..Default::default()
    };
    let mut update_bss = false;
    if policy.station_mode && policy.state_running && current_bssid == Some(bssid) {
        if old_erp != new_erp {
            update.protection_changed = Some(
                (policy.current_mode == crate::PhyMode::G
                    || (matches!(policy.current_mode, crate::PhyMode::N | crate::PhyMode::Ax)
                        && channel <= 14))
                    && new_erp & ERP_USE_PROTECTION != 0,
            );
            update_bss = true;
        }
        if valid_htop
            && (bss_before.ht_operation.htop1 & 0x0003) != (new_htop1 & 0x0003)
            && bss_before.ht_caps.flags & crate::NODE_HT != 0
        {
            update.ht_protection_changed = true;
            update_bss = true;
        }
        if bss_before.ht_caps.flags & crate::NODE_HT != 0
            && policy.local_ht_caps & HTCAP_CBW20_40 != 0
        {
            update.forty_mhz_changed = bss_before.ht_operation.htop0 & 0x04 != new_htop0 & 0x04
                || bss_before.ht_operation.htop0 & 0x03 != new_htop0 & 0x03;
            update_bss = true;
        }
        if bss_before.access_point.capability_info & CAPINFO_SHORT_SLOTTIME
            != capability & CAPINFO_SHORT_SLOTTIME
        {
            update.short_slot_changed = Some(
                policy.current_mode == crate::PhyMode::A
                    || capability & CAPINFO_SHORT_SLOTTIME != 0,
            );
        }
        if ies.tim.is_some() && bss_before.dtim_period != new_dtim_period {
            update.dtim_changed = true;
            update_bss = true;
        }
        update.reset_management_timer = !policy.background_scan;
    }
    if update_bss {
        table.bss_node.erp = new_erp;
        table.bss_node.ht_operation.htop0 = new_htop0;
        table.bss_node.ht_operation.htop1 = new_htop1;
        table.bss_node.dtim_count = new_dtim_count;
        table.bss_node.dtim_period = new_dtim_period;
    }
    Ok(update)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy<'a>(channels: &'a [u8], rates: &'a RateSet) -> BeaconPolicy<'a> {
        BeaconPolicy {
            active_channels: channels,
            scan_all: false,
            background_scan: false,
            state_scanning: true,
            state_running: false,
            local_channel_160_allowed: false,
            current_channel: 1,
            current_mode: crate::PhyMode::B,
            local_ht_caps: 0,
            local_vht_caps: 0,
            local_vht_tx_max_lgi_mbps: 0,
            station_mode: true,
            local_rsn_capable: false,
            local_qos_enabled: false,
            local_uapsd_enabled: false,
            local_uapsd_access_categories: 0,
            local_uapsd_max_service_period: 0,
            local_rates: rates,
            fixed_rate: None,
        }
    }

    fn beacon_frame(subtype: u8) -> Vec<u8> {
        let mut frame = vec![0; MAC_HEADER_LEN + FIXED_BEACON_LEN];
        frame[0] = subtype;
        frame[10..16].copy_from_slice(&[0x02, 0, 0, 0, 0, 1]);
        frame[16..22].copy_from_slice(&[0x02, 0, 0, 0, 0, 1]);
        frame[32..34].copy_from_slice(&100u16.to_le_bytes());
        frame[34..36].copy_from_slice(&CAPINFO_SHORT_SLOTTIME.to_le_bytes());
        frame.extend_from_slice(&[EID_SSID, 3, b'n', b'e', b't']);
        frame.extend_from_slice(&[EID_RATES, 2, 0x82, 0x84]);
        frame.extend_from_slice(&[EID_DS_PARAMS, 1, 1]);
        frame
    }

    #[test]
    fn beacon_updates_scan_node_and_current_bss_effects() {
        let mut table = NodeTable::default();
        let local_rates = RateSet::default();
        let mut policy = policy(&[1], &local_rates);
        policy.state_running = true;
        let update = receive_beacon(
            &mut table,
            &beacon_frame(FC0_SUBTYPE_BEACON),
            BeaconRxInfo {
                receive_channel: Some(1),
                rssi: 42,
                timestamp: 99,
                is_probe_response: false,
            },
            &policy,
            Some([0x02, 0, 0, 0, 0, 1]),
        )
        .unwrap();

        assert!(update.is_new_node);
        assert_eq!(update.channel, 1);
        assert!(update.short_slot_changed.is_some());
        let node = find_node(&table, &[0x02, 0, 0, 0, 0, 1]).unwrap();
        assert_eq!(node.access_point.ssid_len, 3);
        assert_eq!(&node.access_point.ssid[..3], b"net");
        assert_eq!(node.access_point.rssi, 42);
        assert_eq!(node.receive_timestamp, 99);
        assert_eq!(node.beacon_interval, 100);
        assert_eq!(node.access_point.rates.count, 2);
    }

    #[test]
    fn probe_response_refreshes_scan_rssi_on_five_ghz() {
        let mut table = NodeTable::default();
        let local_rates = RateSet::default();
        let mut frame = beacon_frame(FC0_SUBTYPE_BEACON);
        let ds_channel_offset = MAC_HEADER_LEN + FIXED_BEACON_LEN + 2 + 3 + 2 + 2;
        frame[ds_channel_offset + 2] = 36;
        let mut policy = policy(&[36], &local_rates);
        policy.current_channel = 36;
        for (rssi, probe) in [(70, false), (40, true)] {
            frame[0] = if probe {
                FC0_SUBTYPE_PROBE_RESP
            } else {
                FC0_SUBTYPE_BEACON
            };
            receive_beacon(
                &mut table,
                &frame,
                BeaconRxInfo {
                    receive_channel: Some(36),
                    rssi,
                    timestamp: 1,
                    is_probe_response: probe,
                },
                &policy,
                None,
            )
            .unwrap();
        }
        assert_eq!(
            find_node(&table, &[0x02, 0, 0, 0, 0, 1])
                .unwrap()
                .access_point
                .rssi,
            40
        );
    }

    #[test]
    fn malformed_ie_tail_is_counted_without_reading_past_frame() {
        let (ies, malformed) = parse_beacon_ies(&[EID_SSID, 9, b'x', b'y'], 1);
        assert_eq!(malformed, 1);
        assert!(ies.ssid.is_none());
    }

    #[test]
    fn rejects_off_channel_response_outside_scan_all() {
        let mut frame = beacon_frame(FC0_SUBTYPE_BEACON);
        let ds_channel_offset = MAC_HEADER_LEN + FIXED_BEACON_LEN + 2 + 3 + 2 + 2;
        frame[ds_channel_offset + 2] = 6;
        let mut table = NodeTable::default();
        let local_rates = RateSet::default();
        let result = receive_beacon(
            &mut table,
            &frame,
            BeaconRxInfo {
                receive_channel: Some(1),
                rssi: 1,
                timestamp: 0,
                is_probe_response: false,
            },
            &policy(&[1, 6], &local_rates),
            None,
        );
        assert_eq!(result, Err(BeaconError::ChannelMismatch));
    }
}
