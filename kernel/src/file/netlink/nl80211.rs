//! nl80211 generic-netlink family discovery and no-device dump contract.
//!
//! UAPI values are translated from Linux `include/uapi/linux/nl80211.h`
//! version 7.2.3 (ISC-style grant). Copyright 2006-2010 Johannes Berg;
//! Copyright 2008 Michael Wu, Luis Carlos Cobo, Michael Buesch, Jouni Malinen
//! and Colin McCabe; Copyright 2015-2017 Intel Deutschland GmbH; Copyright
//! (C) 2018-2026 Intel Corporation. The complete grant is retained in
//! `kernel/LICENSES/ISC.txt`.

use alloc::format;

use super::*;

pub(super) const FAMILY_ID: u16 = 0x12;
pub(super) const FAMILY_NAME: &str = "nl80211";
const FAMILY_VERSION: u8 = 1;
const FAMILY_MAX_ATTRIBUTE: u32 = 366;

pub(super) const CMD_GET_WIPHY: u8 = 1;
const CMD_NEW_WIPHY: u8 = 3;
pub(super) const CMD_GET_INTERFACE: u8 = 5;
const CMD_NEW_INTERFACE: u8 = 7;
const CMD_GET_SCAN: u8 = 32;
const CMD_TRIGGER_SCAN: u8 = 33;
const CMD_NEW_SCAN_RESULTS: u8 = 34;
const CMD_SCAN_ABORTED: u8 = 35;
const CMD_ABORT_SCAN: u8 = 114;
const NL80211_SCAN_GROUP_MASK: u32 = 1 << 1;
const CMD_GET_REG: u8 = 31;
const CMD_CONNECT: u8 = 46;
const CMD_DISCONNECT: u8 = 48;
const CMD_GET_STATION: u8 = 17;
const CMD_NEW_STATION: u8 = 19;
const CMD_GET_KEY: u8 = 9;
const CMD_SET_KEY: u8 = 10;
const CMD_NEW_KEY: u8 = 11;
const CMD_DEL_KEY: u8 = 12;
const CMD_SET_PMKSA: u8 = 52;
const CMD_DEL_PMKSA: u8 = 53;
const CMD_FLUSH_PMKSA: u8 = 54;
const NL80211_MLME_GROUP_MASK: u32 = 1 << 3;
const ATTR_WIPHY: u16 = 1;
const ATTR_WIPHY_NAME: u16 = 2;
const ATTR_IFINDEX: u16 = 3;
const ATTR_IFNAME: u16 = 4;
const ATTR_IFTYPE: u16 = 5;
const ATTR_MAC: u16 = 6;
const ATTR_KEY_DATA: u16 = 8;
const ATTR_KEY_IDX: u16 = 9;
const ATTR_KEY_CIPHER: u16 = 10;
const ATTR_KEY_SEQ: u16 = 11;
const ATTR_KEY_DEFAULT: u16 = 12;
const ATTR_KEY_DEFAULT_TYPES: u16 = 110;
const KEY_DEFAULT_TYPE_UNICAST: u16 = 1;
const KEY_DEFAULT_TYPE_MULTICAST: u16 = 2;
const ATTR_STA_INFO: u16 = 21;
const ATTR_PMKID: u16 = 85;
const ATTR_STATUS_CODE: u16 = 72;
const ATTR_WIPHY_FREQ: u16 = 38;
const ATTR_CONNECT_IE: u16 = 42;
const ATTR_AUTH_TYPE: u16 = 53;
const ATTR_REASON_CODE: u16 = 54;
const ATTR_SSID: u16 = 52;
const ATTR_CIPHER_SUITES_PAIRWISE: u16 = 73;
const ATTR_CIPHER_SUITE_GROUP: u16 = 74;
const ATTR_WPA_VERSIONS: u16 = 75;
const ATTR_AKM_SUITES: u16 = 76;
const ATTR_REQ_IE: u16 = 77;
const ATTR_RESP_IE: u16 = 78;
const ATTR_BSS: u16 = 47;
const ATTR_SCAN_FREQUENCIES: u16 = 44;
const ATTR_SCAN_SSIDS: u16 = 45;
const ATTR_WIPHY_BANDS: u16 = 22;
const ATTR_SUPPORTED_IFTYPES: u16 = 32;
const ATTR_MAX_NUM_SCAN_SSIDS: u16 = 43;
const ATTR_SUPPORTED_COMMANDS: u16 = 50;
const ATTR_CIPHER_SUITES: u16 = 57;
const ATTR_MAX_NUM_PMKIDS: u16 = 86;
const ATTR_SPLIT_WIPHY_DUMP: u16 = 174;
const ATTR_REG_ALPHA2: u16 = 33;
const BAND_ATTR_FREQS: u16 = 1;
const BAND_ATTR_RATES: u16 = 2;
const BAND_ATTR_HT_MCS_SET: u16 = 3;
const BAND_ATTR_HT_CAPA: u16 = 4;
const BAND_ATTR_HT_AMPDU_FACTOR: u16 = 5;
const BAND_ATTR_HT_AMPDU_DENSITY: u16 = 6;
const BAND_ATTR_VHT_MCS_SET: u16 = 7;
const BAND_ATTR_VHT_CAPA: u16 = 8;
const FREQ_ATTR_FREQ: u16 = 1;
const FREQ_ATTR_NO_IR: u16 = 3;
const BSS_ATTR_BSSID: u16 = 1;
const BSS_ATTR_FREQUENCY: u16 = 2;
const BSS_ATTR_TSF: u16 = 3;
const BSS_ATTR_BEACON_INTERVAL: u16 = 4;
const BSS_ATTR_CAPABILITY: u16 = 5;
const BSS_ATTR_INFORMATION_ELEMENTS: u16 = 6;
const BSS_ATTR_SIGNAL_MBM: u16 = 7;
const BSS_ATTR_SEEN_MS_AGO: u16 = 10;
const STA_INFO_SIGNAL: u16 = 7;
const BITRATE_ATTR_RATE: u16 = 1;
const BITRATE_ATTR_2GHZ_SHORTPREAMBLE: u16 = 2;
const IFTYPE_STATION: u32 = 2;
const IFTYPE_STATION_ATTR: u16 = 2;
const NLM_F_DUMP: u16 = 0x0300;

const CTRL_ATTR_MCAST_GROUPS: u16 = 7;
const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;
const NLA_F_NESTED: u16 = 1 << 15;

const MULTICAST_GROUPS: [&str; 7] = [
    "config",
    "scan",
    "regulatory",
    "mlme",
    "vendor",
    "nan",
    "testmode",
];

pub(super) fn family_message(request: &NlMsgHdr, port_id: u32) -> Vec<u8> {
    axnet::register_wireless_scan_event_callback(publish_wireless_scan_event);
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: CTRL_CMD_NEWFAMILY,
        version: 2,
        reserved: 0,
    });
    push_attr(&mut payload, CTRL_ATTR_FAMILY_ID, &FAMILY_ID.to_ne_bytes());
    push_attr_string(&mut payload, CTRL_ATTR_FAMILY_NAME, FAMILY_NAME);
    push_attr(
        &mut payload,
        CTRL_ATTR_VERSION,
        &u32::from(FAMILY_VERSION).to_ne_bytes(),
    );
    push_attr(&mut payload, CTRL_ATTR_HDRSIZE, &0u32.to_ne_bytes());
    push_attr(
        &mut payload,
        CTRL_ATTR_MAXATTR,
        &FAMILY_MAX_ATTRIBUTE.to_ne_bytes(),
    );
    let mut groups = Vec::new();
    for (index, name) in MULTICAST_GROUPS.iter().enumerate() {
        let mut group = Vec::new();
        push_attr_string(&mut group, CTRL_ATTR_MCAST_GRP_NAME, name);
        push_attr(
            &mut group,
            CTRL_ATTR_MCAST_GRP_ID,
            &((index as u32) + 1).to_ne_bytes(),
        );
        push_attr(&mut groups, (index as u16) + 1 | NLA_F_NESTED, &group);
    }
    push_attr(&mut payload, CTRL_ATTR_MCAST_GROUPS | NLA_F_NESTED, &groups);
    wiremsg::netlink_message(request, port_id, GENL_ID_CTRL, payload)
}

fn publish_wireless_scan_event(ifindex: u32, event: axnet::WirelessScanEvent) {
    let message = scan_event_message(ifindex, event);
    super::queue_nl80211_multicast(message, NL80211_SCAN_GROUP_MASK);
}

fn scan_event_message(ifindex: u32, event: axnet::WirelessScanEvent) -> Vec<u8> {
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: match event {
            axnet::WirelessScanEvent::Results => CMD_NEW_SCAN_RESULTS,
            axnet::WirelessScanEvent::Aborted => CMD_SCAN_ABORTED,
        },
        version: FAMILY_VERSION,
        reserved: 0,
    });
    push_attr(&mut payload, ATTR_IFINDEX, &ifindex.to_ne_bytes());
    let request = NlMsgHdr {
        nlmsg_len: (size_of::<NlMsgHdr>() + payload.len()) as u32,
        nlmsg_type: FAMILY_ID,
        nlmsg_flags: 0,
        nlmsg_seq: 0,
        nlmsg_pid: 0,
    };
    nl80211_message(&request, 0, FAMILY_ID, payload, false)
}

fn publish_connect_event(ifindex: u32, station: &axnet::WirelessStationInfo) {
    super::queue_nl80211_multicast(
        connect_event_message(ifindex, station),
        NL80211_MLME_GROUP_MASK,
    );
}

fn publish_disconnect_event(ifindex: u32, bssid: [u8; 6], reason: u16) {
    super::queue_nl80211_multicast(
        disconnect_event_message(ifindex, bssid, reason),
        NL80211_MLME_GROUP_MASK,
    );
}

fn disconnect_event_message(ifindex: u32, bssid: [u8; 6], reason: u16) -> Vec<u8> {
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: CMD_DISCONNECT,
        version: FAMILY_VERSION,
        reserved: 0,
    });
    push_attr(&mut payload, ATTR_IFINDEX, &ifindex.to_ne_bytes());
    push_attr(&mut payload, ATTR_MAC, &bssid);
    push_attr(&mut payload, ATTR_REASON_CODE, &reason.to_ne_bytes());
    let request = NlMsgHdr {
        nlmsg_len: (size_of::<NlMsgHdr>() + payload.len()) as u32,
        nlmsg_type: FAMILY_ID,
        nlmsg_flags: 0,
        nlmsg_seq: 0,
        nlmsg_pid: 0,
    };
    nl80211_message(&request, 0, FAMILY_ID, payload, false)
}

fn connect_event_message(ifindex: u32, station: &axnet::WirelessStationInfo) -> Vec<u8> {
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: CMD_CONNECT,
        version: FAMILY_VERSION,
        reserved: 0,
    });
    push_attr(&mut payload, ATTR_IFINDEX, &ifindex.to_ne_bytes());
    push_attr(&mut payload, ATTR_MAC, &station.bssid);
    push_attr(&mut payload, ATTR_STATUS_CODE, &0u16.to_ne_bytes());
    push_attr(
        &mut payload,
        ATTR_WIPHY_FREQ,
        &station.frequency_mhz.to_ne_bytes(),
    );
    if !station.request_ies.is_empty() {
        push_attr(&mut payload, ATTR_REQ_IE, &station.request_ies);
    }
    if !station.response_ies.is_empty() {
        push_attr(&mut payload, ATTR_RESP_IE, &station.response_ies);
    }
    let request = NlMsgHdr {
        nlmsg_len: (size_of::<NlMsgHdr>() + payload.len()) as u32,
        nlmsg_type: FAMILY_ID,
        nlmsg_flags: 0,
        nlmsg_seq: 0,
        nlmsg_pid: 0,
    };
    let message = nl80211_message(&request, 0, FAMILY_ID, payload, false);
    message
}

/// Handle the wiphy/interface dump operations used by `iw dev` and `iw phy`.
pub(super) fn handle(
    socket: &NetlinkSocket,
    permit: &mut NetlinkWritePermit<'_>,
    header: &NlMsgHdr,
    payload: &[u8],
) -> AxResult {
    if payload.len() < size_of::<GenlMsgHdr>() {
        return Err(AxError::InvalidInput);
    }
    let request = read_unaligned::<GenlMsgHdr>(payload)?;
    if request.version > FAMILY_VERSION || request.reserved != 0 {
        return Err(AxError::InvalidInput);
    }
    if !matches!(
        request.cmd,
        CMD_GET_WIPHY
            | CMD_GET_INTERFACE
            | CMD_GET_SCAN
            | CMD_TRIGGER_SCAN
            | CMD_ABORT_SCAN
            | CMD_GET_REG
            | CMD_CONNECT
            | CMD_DISCONNECT
            | CMD_GET_STATION
            | CMD_GET_KEY
            | CMD_SET_KEY
            | CMD_NEW_KEY
            | CMD_DEL_KEY
            | CMD_SET_PMKSA
            | CMD_DEL_PMKSA
            | CMD_FLUSH_PMKSA
    ) {
        return Err(AxError::OperationNotSupported);
    }
    let attributes = &payload[size_of::<GenlMsgHdr>()..];
    let dump = header.nlmsg_flags & NLM_F_DUMP != 0;
    let interfaces = axnet::wireless_interfaces();
    let port_id = permit.port_id();
    let mut records = Vec::new();
    if request.cmd == CMD_TRIGGER_SCAN {
        if dump {
            return Err(AxError::InvalidInput);
        }
        let (ifindex, scan) = parse_scan_request(attributes)?;
        if !interfaces
            .iter()
            .any(|interface| interface.ifindex == ifindex)
        {
            return Err(AxError::NotFound);
        }
        axnet::trigger_wireless_scan(ifindex, &scan)?;
        return Ok(());
    }
    if request.cmd == CMD_ABORT_SCAN {
        if dump {
            return Err(AxError::InvalidInput);
        }
        let selectors = parse_selectors(attributes)?;
        let ifindex = selectors.ifindex.ok_or(AxError::InvalidInput)?;
        if !interfaces
            .iter()
            .any(|interface| interface.ifindex == ifindex)
        {
            return Err(AxError::NotFound);
        }
        axnet::abort_wireless_scan(ifindex)?;
        return Ok(());
    }
    if request.cmd == CMD_CONNECT {
        if dump {
            return Err(AxError::InvalidInput);
        }
        let (ifindex, connect) = parse_connect_request(attributes)?;
        if !interfaces
            .iter()
            .any(|interface| interface.ifindex == ifindex)
        {
            return Err(AxError::NotFound);
        }
        axnet::connect_wireless(ifindex, &connect)?;
        let station = axnet::wireless_station_info(ifindex)?;
        publish_connect_event(ifindex, &station);
        return Ok(());
    }
    if request.cmd == CMD_DISCONNECT {
        if dump {
            return Err(AxError::InvalidInput);
        }
        let (ifindex, reason) = parse_disconnect_request(attributes)?;
        if !interfaces
            .iter()
            .any(|interface| interface.ifindex == ifindex)
        {
            return Err(AxError::NotFound);
        }
        let bssid = axnet::wireless_station_info(ifindex)
            .ok()
            .map(|station| station.bssid);
        axnet::disconnect_wireless(ifindex, reason)?;
        if let Some(bssid) = bssid {
            publish_disconnect_event(ifindex, bssid, reason);
        }
        return Ok(());
    }
    if request.cmd == CMD_GET_STATION {
        let (ifindex, address) = parse_station_request(attributes)?;
        let interface = interfaces
            .iter()
            .find(|interface| interface.ifindex == ifindex)
            .ok_or(AxError::NotFound)?;
        match axnet::wireless_station_info(ifindex) {
            Ok(station) if address.is_none_or(|address| address == station.bssid) => {
                records.push(station_message(header, port_id, interface, &station, dump));
            }
            Ok(_) | Err(AxError::NotFound) if dump => {}
            _ => return Err(AxError::NotFound),
        }
        for record in records {
            socket.enqueue_kernel_permitted(permit, record);
        }
        if dump {
            socket.enqueue_kernel_permitted(permit, empty_dump_response(header, port_id));
        }
        return Ok(());
    }
    if matches!(
        request.cmd,
        CMD_NEW_KEY | CMD_SET_KEY | CMD_GET_KEY | CMD_DEL_KEY
    ) {
        if dump {
            return Err(AxError::InvalidInput);
        }
        let mut operation = match request.cmd {
            CMD_NEW_KEY => axnet::WirelessKeyOperation::Install,
            CMD_SET_KEY => axnet::WirelessKeyOperation::SetDefault {
                unicast: false,
                multicast: true,
            },
            CMD_GET_KEY => axnet::WirelessKeyOperation::GetSequence,
            CMD_DEL_KEY => axnet::WirelessKeyOperation::Delete,
            _ => unreachable!(),
        };
        let (ifindex, key) = parse_key_request(attributes, request.cmd)?;
        if request.cmd == CMD_SET_KEY {
            operation = axnet::WirelessKeyOperation::SetDefault {
                unicast: key.default_unicast || (key.peer.is_some() && !key.default_multicast),
                multicast: key.default_multicast || (key.peer.is_none() && !key.default_unicast),
            };
        }
        if !interfaces
            .iter()
            .any(|interface| interface.ifindex == ifindex)
        {
            return Err(AxError::NotFound);
        }
        let result = axnet::wireless_key_operation(ifindex, operation, &key)?;
        if let Some(info) = result {
            socket.enqueue_kernel_permitted(
                permit,
                key_message(header, port_id, ifindex, &key, &info),
            );
        }
        return Ok(());
    }
    if matches!(request.cmd, CMD_SET_PMKSA | CMD_DEL_PMKSA | CMD_FLUSH_PMKSA) {
        if dump {
            return Err(AxError::InvalidInput);
        }
        let (ifindex, _peer, _pmkid) = parse_pmksa_request(attributes, request.cmd)?;
        if !interfaces
            .iter()
            .any(|interface| interface.ifindex == ifindex)
        {
            return Err(AxError::NotFound);
        }
        // The userspace supplicant owns PMKSA and includes a cached PMKID in
        // its next CONNECT request IE; no kernel/firmware cache is maintained.
        return Ok(());
    }
    let selectors = parse_selectors(attributes)?;
    match request.cmd {
        CMD_GET_REG => {
            if selectors.wiphy.is_some_and(|wiphy| {
                !interfaces
                    .iter()
                    .any(|interface| interface.phy_index == wiphy)
            }) {
                return Err(AxError::NotFound);
            }
            socket.enqueue_kernel_permitted(permit, regulatory_message(header, port_id));
            return Ok(());
        }
        CMD_GET_SCAN => {
            if !dump {
                return Err(AxError::InvalidInput);
            }
            let scan_interface = interfaces.iter().find(|interface| {
                selectors
                    .ifindex
                    .is_none_or(|ifindex| interface.ifindex == ifindex)
                    && selectors
                        .ifname
                        .as_deref()
                        .is_none_or(|name| interface.name == name)
            });
            let scan_interface = scan_interface.ok_or(AxError::NotFound)?;
            for result in axnet::wireless_scan_results(scan_interface.ifindex)? {
                records.push(scan_bss_message(
                    header,
                    port_id,
                    scan_interface.ifindex,
                    &result,
                ));
            }
        }
        CMD_GET_INTERFACE => {
            for interface in interfaces.iter().filter(|interface| {
                selectors
                    .ifindex
                    .is_none_or(|ifindex| interface.ifindex == ifindex)
                    && selectors
                        .ifname
                        .as_deref()
                        .is_none_or(|name| interface.name == name)
            }) {
                records.push(interface_message(header, port_id, interface, dump));
            }
        }
        CMD_GET_WIPHY => {
            let mut written_phys = alloc::collections::BTreeSet::new();
            for interface in interfaces.iter().filter(|interface| {
                selectors
                    .wiphy
                    .is_none_or(|wiphy| interface.phy_index == wiphy)
                    && selectors
                        .wiphy_name
                        .as_deref()
                        .is_none_or(|name| format!("phy{}", interface.phy_index) == name)
            }) {
                if written_phys.insert(interface.phy_index) {
                    records.push(wiphy_message(header, port_id, interface, dump));
                }
            }
        }
        _ => unreachable!(),
    }
    if records.is_empty() && !dump {
        return Err(AxError::NotFound);
    }
    for record in records {
        socket.enqueue_kernel_permitted(permit, record);
    }
    if dump {
        socket.enqueue_kernel_permitted(permit, empty_dump_response(header, port_id));
    }
    Ok(())
}

fn parse_scan_request(attributes: &[u8]) -> AxResult<(u32, axnet::WirelessScanRequest)> {
    let mut ifindex = None;
    let mut ssid = None;
    let mut frequencies = Vec::new();
    for_each_rtattr(attributes, |kind, value| match kind {
        ATTR_IFINDEX if ifindex.is_none() && value.len() == 4 => {
            ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_SCAN_SSIDS if ssid.is_none() => {
            let mut count = 0;
            for_each_rtattr(value, |_, bytes| {
                count += 1;
                if count > 1 || bytes.len() > 32 {
                    return Err(AxError::InvalidInput);
                }
                ssid = Some(bytes.to_vec());
                Ok(())
            })?;
            Ok(())
        }
        ATTR_SCAN_FREQUENCIES => for_each_rtattr(value, |_, bytes| {
            if bytes.len() != 4 {
                return Err(AxError::InvalidInput);
            }
            let frequency = u32::from_ne_bytes(bytes.try_into().unwrap());
            if frequency == 0 || frequencies.contains(&frequency) {
                return Err(AxError::InvalidInput);
            }
            frequencies.push(frequency);
            Ok(())
        }),
        ATTR_IFINDEX | ATTR_SCAN_SSIDS => Err(AxError::InvalidInput),
        _ => Err(AxError::OperationNotSupported),
    })?;
    Ok((
        ifindex.ok_or(AxError::InvalidInput)?,
        axnet::WirelessScanRequest {
            ssid: ssid.unwrap_or_default(),
            frequencies_mhz: frequencies,
        },
    ))
}

fn parse_connect_request(attributes: &[u8]) -> AxResult<(u32, axnet::WirelessConnectRequest)> {
    let mut ifindex = None;
    let mut ssid = None;
    let mut bssid = None;
    let mut authentication_type = None;
    let mut wpa_versions = None;
    let mut pairwise_ciphers = Vec::new();
    let mut pairwise_present = false;
    let mut group_cipher = None;
    let mut akm_suites = Vec::new();
    let mut akm_present = false;
    let mut information_elements = None;
    for_each_rtattr(attributes, |kind, value| match kind {
        ATTR_IFINDEX if ifindex.is_none() && value.len() == 4 => {
            ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_SSID if ssid.is_none() && value.len() <= 32 => {
            ssid = Some(value.to_vec());
            Ok(())
        }
        ATTR_MAC if bssid.is_none() && value.len() == 6 => {
            bssid = Some(value.try_into().unwrap());
            Ok(())
        }
        ATTR_AUTH_TYPE if authentication_type.is_none() && value.len() == 4 => {
            authentication_type = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_WPA_VERSIONS if wpa_versions.is_none() && value.len() == 4 => {
            wpa_versions = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_CIPHER_SUITE_GROUP if group_cipher.is_none() && value.len() == 4 => {
            group_cipher = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_CIPHER_SUITES_PAIRWISE if !pairwise_present => {
            pairwise_present = true;
            for_each_rtattr(value, |_, suite| {
                if suite.len() != 4
                    || pairwise_ciphers.contains(&u32::from_ne_bytes(suite.try_into().unwrap()))
                {
                    return Err(AxError::InvalidInput);
                }
                pairwise_ciphers.push(u32::from_ne_bytes(suite.try_into().unwrap()));
                Ok(())
            })
        }
        ATTR_AKM_SUITES if !akm_present => {
            akm_present = true;
            for_each_rtattr(value, |_, suite| {
                if suite.len() != 4
                    || akm_suites.contains(&u32::from_ne_bytes(suite.try_into().unwrap()))
                {
                    return Err(AxError::InvalidInput);
                }
                akm_suites.push(u32::from_ne_bytes(suite.try_into().unwrap()));
                Ok(())
            })
        }
        ATTR_CONNECT_IE if information_elements.is_none() && value.len() <= 4096 => {
            information_elements = Some(value.to_vec());
            Ok(())
        }
        ATTR_IFINDEX
        | ATTR_SSID
        | ATTR_MAC
        | ATTR_AUTH_TYPE
        | ATTR_WPA_VERSIONS
        | ATTR_CIPHER_SUITE_GROUP
        | ATTR_CIPHER_SUITES_PAIRWISE
        | ATTR_AKM_SUITES
        | ATTR_CONNECT_IE => Err(AxError::InvalidInput),
        _ => Err(AxError::OperationNotSupported),
    })?;
    if pairwise_ciphers.len() > 8 || akm_suites.len() > 8 {
        return Err(AxError::InvalidInput);
    }
    Ok((
        ifindex.ok_or(AxError::InvalidInput)?,
        axnet::WirelessConnectRequest {
            ssid: ssid.ok_or(AxError::InvalidInput)?,
            bssid,
            authentication_type: authentication_type.unwrap_or(0),
            wpa_versions: wpa_versions.unwrap_or(0),
            pairwise_ciphers,
            group_cipher,
            akm_suites,
            information_elements: information_elements.unwrap_or_default(),
        },
    ))
}

fn parse_disconnect_request(attributes: &[u8]) -> AxResult<(u32, u16)> {
    let mut ifindex = None;
    let mut reason = None;
    for_each_rtattr(attributes, |kind, value| match kind {
        ATTR_IFINDEX if ifindex.is_none() && value.len() == 4 => {
            ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_REASON_CODE if reason.is_none() && value.len() == 2 => {
            reason = Some(u16::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_IFINDEX | ATTR_REASON_CODE => Err(AxError::InvalidInput),
        _ => Err(AxError::OperationNotSupported),
    })?;
    Ok((
        ifindex.ok_or(AxError::InvalidInput)?,
        reason.unwrap_or_default(),
    ))
}

fn parse_station_request(attributes: &[u8]) -> AxResult<(u32, Option<[u8; 6]>)> {
    let mut ifindex = None;
    let mut address = None;
    for_each_rtattr(attributes, |kind, value| match kind {
        ATTR_IFINDEX if ifindex.is_none() && value.len() == 4 => {
            ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_MAC if address.is_none() && value.len() == 6 => {
            address = Some(value.try_into().unwrap());
            Ok(())
        }
        ATTR_IFINDEX | ATTR_MAC => Err(AxError::InvalidInput),
        _ => Err(AxError::OperationNotSupported),
    })?;
    Ok((ifindex.ok_or(AxError::InvalidInput)?, address))
}

fn parse_key_request(attributes: &[u8], command: u8) -> AxResult<(u32, axnet::WirelessKeyConfig)> {
    let mut ifindex = None;
    let mut index = None;
    let mut cipher = None;
    let mut key_data = None;
    let mut sequence = None;
    let mut peer = None;
    let mut default_flag = false;
    let mut default_types_present = false;
    let mut default_unicast = false;
    let mut default_multicast = false;
    for_each_rtattr(attributes, |kind, value| match kind {
        ATTR_IFINDEX if ifindex.is_none() && value.len() == 4 => {
            ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_KEY_IDX if index.is_none() && value.len() == 1 => {
            index = Some(value[0]);
            Ok(())
        }
        ATTR_KEY_CIPHER if cipher.is_none() && value.len() == 4 => {
            cipher = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_KEY_DATA if key_data.is_none() && value.len() <= 64 => {
            key_data = Some(value.to_vec());
            Ok(())
        }
        ATTR_KEY_SEQ if sequence.is_none() && !value.is_empty() && value.len() <= 8 => {
            sequence = Some(value.to_vec());
            Ok(())
        }
        ATTR_MAC if peer.is_none() && value.len() == 6 => {
            peer = Some(value.try_into().unwrap());
            Ok(())
        }
        ATTR_KEY_DEFAULT if !default_flag && value.is_empty() => {
            default_flag = true;
            Ok(())
        }
        ATTR_KEY_DEFAULT_TYPES if !default_types_present => {
            default_types_present = true;
            for_each_rtattr(value, |kind, flag| {
                if !flag.is_empty() {
                    return Err(AxError::InvalidInput);
                }
                match kind {
                    KEY_DEFAULT_TYPE_UNICAST if !default_unicast => default_unicast = true,
                    KEY_DEFAULT_TYPE_MULTICAST if !default_multicast => default_multicast = true,
                    KEY_DEFAULT_TYPE_UNICAST | KEY_DEFAULT_TYPE_MULTICAST => {
                        return Err(AxError::InvalidInput);
                    }
                    _ => return Err(AxError::OperationNotSupported),
                }
                Ok(())
            })
        }
        ATTR_IFINDEX
        | ATTR_KEY_IDX
        | ATTR_KEY_CIPHER
        | ATTR_KEY_DATA
        | ATTR_KEY_SEQ
        | ATTR_MAC
        | ATTR_KEY_DEFAULT
        | ATTR_KEY_DEFAULT_TYPES => Err(AxError::InvalidInput),
        _ => Err(AxError::OperationNotSupported),
    })?;
    let index = index.ok_or(AxError::InvalidInput)?;
    if index > 3 {
        return Err(AxError::InvalidInput);
    }
    match command {
        CMD_NEW_KEY if key_data.is_some() && cipher.is_some() && !default_flag => {}
        CMD_SET_KEY if key_data.is_none() && cipher.is_none() && default_flag => {}
        CMD_GET_KEY | CMD_DEL_KEY if key_data.is_none() && cipher.is_none() && !default_flag => {}
        _ => return Err(AxError::InvalidInput),
    }
    Ok((
        ifindex.ok_or(AxError::InvalidInput)?,
        axnet::WirelessKeyConfig {
            index,
            cipher_suite: cipher.unwrap_or_default(),
            peer: peer.filter(|address| *address != [0xff; 6]),
            key_data: key_data.unwrap_or_default(),
            sequence: sequence.unwrap_or_default(),
            default_unicast,
            default_multicast,
        },
    ))
}

fn parse_pmksa_request(
    attributes: &[u8],
    command: u8,
) -> AxResult<(u32, Option<[u8; 6]>, Option<[u8; 16]>)> {
    let mut ifindex = None;
    let mut peer = None;
    let mut pmkid = None;
    for_each_rtattr(attributes, |kind, value| match kind {
        ATTR_IFINDEX if ifindex.is_none() && value.len() == 4 => {
            ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_MAC if peer.is_none() && value.len() == 6 => {
            peer = Some(value.try_into().unwrap());
            Ok(())
        }
        ATTR_PMKID if pmkid.is_none() && value.len() == 16 => {
            pmkid = Some(value.try_into().unwrap());
            Ok(())
        }
        ATTR_IFINDEX | ATTR_MAC | ATTR_PMKID => Err(AxError::InvalidInput),
        _ => Err(AxError::OperationNotSupported),
    })?;
    match command {
        CMD_SET_PMKSA if peer.is_some() && pmkid.is_some() => {}
        CMD_DEL_PMKSA if peer.is_some() => {}
        CMD_FLUSH_PMKSA if peer.is_none() && pmkid.is_none() => {}
        _ => return Err(AxError::InvalidInput),
    }
    Ok((ifindex.ok_or(AxError::InvalidInput)?, peer, pmkid))
}

fn scan_bss_message(
    request: &NlMsgHdr,
    port_id: u32,
    ifindex: u32,
    bss: &axnet::WirelessBssInfo,
) -> Vec<u8> {
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: CMD_NEW_SCAN_RESULTS,
        version: FAMILY_VERSION,
        reserved: 0,
    });
    push_attr(&mut payload, ATTR_IFINDEX, &ifindex.to_ne_bytes());
    let mut attributes = Vec::new();
    push_attr(&mut attributes, BSS_ATTR_BSSID, &bss.bssid);
    push_attr(
        &mut attributes,
        BSS_ATTR_FREQUENCY,
        &bss.frequency_mhz.to_ne_bytes(),
    );
    push_attr(&mut attributes, BSS_ATTR_TSF, &bss.timestamp.to_ne_bytes());
    push_attr(
        &mut attributes,
        BSS_ATTR_BEACON_INTERVAL,
        &bss.beacon_interval.to_ne_bytes(),
    );
    push_attr(
        &mut attributes,
        BSS_ATTR_CAPABILITY,
        &bss.capability.to_ne_bytes(),
    );
    push_attr(
        &mut attributes,
        BSS_ATTR_INFORMATION_ELEMENTS,
        &bss.information_elements,
    );
    push_attr(
        &mut attributes,
        BSS_ATTR_SIGNAL_MBM,
        &bss.signal_mbm.to_ne_bytes(),
    );
    push_attr(&mut attributes, BSS_ATTR_SEEN_MS_AGO, &0u32.to_ne_bytes());
    push_attr(&mut payload, ATTR_BSS | NLA_F_NESTED, &attributes);
    nl80211_message(request, port_id, FAMILY_ID, payload, true)
}

fn station_message(
    request: &NlMsgHdr,
    port_id: u32,
    interface: &axnet::WirelessInterfaceInfo,
    station: &axnet::WirelessStationInfo,
    multipart: bool,
) -> Vec<u8> {
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: CMD_NEW_STATION,
        version: FAMILY_VERSION,
        reserved: 0,
    });
    push_attr(&mut payload, ATTR_IFINDEX, &interface.ifindex.to_ne_bytes());
    push_attr(&mut payload, ATTR_MAC, &station.bssid);
    let mut statistics = Vec::new();
    let signal_dbm = (station.signal_mbm / 100).clamp(-127, 0) as i8;
    push_attr(&mut statistics, STA_INFO_SIGNAL, &[signal_dbm as u8]);
    push_attr(&mut payload, ATTR_STA_INFO | NLA_F_NESTED, &statistics);
    nl80211_message(request, port_id, FAMILY_ID, payload, multipart)
}

fn key_message(
    request: &NlMsgHdr,
    port_id: u32,
    ifindex: u32,
    key: &axnet::WirelessKeyConfig,
    info: &axnet::WirelessKeyInfo,
) -> Vec<u8> {
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: CMD_NEW_KEY,
        version: FAMILY_VERSION,
        reserved: 0,
    });
    push_attr(&mut payload, ATTR_IFINDEX, &ifindex.to_ne_bytes());
    push_attr(&mut payload, ATTR_KEY_IDX, &[key.index]);
    push_attr(
        &mut payload,
        ATTR_KEY_CIPHER,
        &info.cipher_suite.to_ne_bytes(),
    );
    if !info.sequence.is_empty() {
        push_attr(&mut payload, ATTR_KEY_SEQ, &info.sequence);
    }
    if let Some(peer) = key.peer {
        push_attr(&mut payload, ATTR_MAC, &peer);
    }
    nl80211_message(request, port_id, FAMILY_ID, payload, false)
}

fn empty_dump_response(request: &NlMsgHdr, port_id: u32) -> Vec<u8> {
    wiremsg::done_message(request, port_id)
}

fn regulatory_message(request: &NlMsgHdr, port_id: u32) -> Vec<u8> {
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: CMD_GET_REG,
        version: FAMILY_VERSION,
        reserved: 0,
    });
    push_attr_string(&mut payload, ATTR_REG_ALPHA2, "00");
    nl80211_message(request, port_id, FAMILY_ID, payload, false)
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Selectors {
    wiphy: Option<u32>,
    ifindex: Option<u32>,
    wiphy_name: Option<String>,
    ifname: Option<String>,
}

fn parse_selectors(attributes: &[u8]) -> AxResult<Selectors> {
    let mut selectors = Selectors::default();
    for_each_rtattr(attributes, |kind, value| match kind {
        ATTR_WIPHY if selectors.wiphy.is_none() && value.len() == 4 => {
            selectors.wiphy = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_IFINDEX if selectors.ifindex.is_none() && value.len() == 4 => {
            selectors.ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            Ok(())
        }
        ATTR_WIPHY_NAME if selectors.wiphy_name.is_none() => {
            selectors.wiphy_name = Some(decode_link_name(value)?);
            Ok(())
        }
        ATTR_IFNAME if selectors.ifname.is_none() => {
            selectors.ifname = Some(decode_link_name(value)?);
            Ok(())
        }
        ATTR_SPLIT_WIPHY_DUMP if value.is_empty() => Ok(()),
        _ => Err(AxError::InvalidInput),
    })?;
    Ok(selectors)
}

fn interface_message(
    request: &NlMsgHdr,
    port_id: u32,
    interface: &axnet::WirelessInterfaceInfo,
    multipart: bool,
) -> Vec<u8> {
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: CMD_NEW_INTERFACE,
        version: FAMILY_VERSION,
        reserved: 0,
    });
    push_attr(&mut payload, ATTR_WIPHY, &interface.phy_index.to_ne_bytes());
    push_attr(&mut payload, ATTR_IFINDEX, &interface.ifindex.to_ne_bytes());
    push_attr_string(&mut payload, ATTR_IFNAME, &interface.name);
    push_attr(&mut payload, ATTR_IFTYPE, &IFTYPE_STATION.to_ne_bytes());
    push_attr(&mut payload, ATTR_MAC, &interface.mac_address);
    nl80211_message(request, port_id, FAMILY_ID, payload, multipart)
}

fn wiphy_message(
    request: &NlMsgHdr,
    port_id: u32,
    interface: &axnet::WirelessInterfaceInfo,
    multipart: bool,
) -> Vec<u8> {
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: CMD_NEW_WIPHY,
        version: FAMILY_VERSION,
        reserved: 0,
    });
    push_attr(&mut payload, ATTR_WIPHY, &interface.phy_index.to_ne_bytes());
    push_attr_string(
        &mut payload,
        ATTR_WIPHY_NAME,
        &format!("phy{}", interface.phy_index),
    );
    let mut interface_types = Vec::new();
    push_attr(&mut interface_types, IFTYPE_STATION_ATTR, &[]);
    push_attr(
        &mut payload,
        ATTR_SUPPORTED_IFTYPES | NLA_F_NESTED,
        &interface_types,
    );
    push_attr(&mut payload, ATTR_MAX_NUM_SCAN_SSIDS, &[1]);
    push_attr(
        &mut payload,
        ATTR_CIPHER_SUITES,
        &0x000fac04u32.to_ne_bytes(),
    );
    push_attr(&mut payload, ATTR_MAX_NUM_PMKIDS, &0u32.to_ne_bytes());
    let mut supported_commands = Vec::new();
    for (index, command) in [
        CMD_GET_WIPHY,
        CMD_GET_INTERFACE,
        CMD_TRIGGER_SCAN,
        CMD_ABORT_SCAN,
        CMD_GET_SCAN,
        CMD_GET_REG,
        CMD_CONNECT,
        CMD_DISCONNECT,
        CMD_GET_STATION,
        CMD_GET_KEY,
        CMD_SET_KEY,
        CMD_NEW_KEY,
        CMD_DEL_KEY,
    ]
    .into_iter()
    .enumerate()
    {
        push_attr(
            &mut supported_commands,
            (index + 1) as u16,
            &u32::from(command).to_ne_bytes(),
        );
    }
    push_attr(
        &mut payload,
        ATTR_SUPPORTED_COMMANDS | NLA_F_NESTED,
        &supported_commands,
    );
    let mut bands = Vec::new();
    for (band_id, is_2ghz) in [(0u16, true), (1u16, false)] {
        let mut frequencies = Vec::new();
        for (index, frequency) in interface
            .frequencies
            .iter()
            .filter(|frequency| (frequency.frequency_mhz < 3000) == is_2ghz)
            .enumerate()
        {
            let mut attributes = Vec::new();
            push_attr(
                &mut attributes,
                FREQ_ATTR_FREQ,
                &frequency.frequency_mhz.to_ne_bytes(),
            );
            if frequency.no_ir {
                push_attr(&mut attributes, FREQ_ATTR_NO_IR, &[]);
            }
            push_attr(&mut frequencies, (index + 1) as u16, &attributes);
        }
        if !frequencies.is_empty() {
            let mut attributes = Vec::new();
            push_attr(
                &mut attributes,
                BAND_ATTR_FREQS | NLA_F_NESTED,
                &frequencies,
            );
            append_supported_rates(&mut attributes, is_2ghz);
            if let Some(ht) = interface.phy_capabilities.ht {
                push_attr(&mut attributes, BAND_ATTR_HT_MCS_SET, &ht.mcs_set);
                push_attr(
                    &mut attributes,
                    BAND_ATTR_HT_CAPA,
                    &ht.capability.to_ne_bytes(),
                );
                push_attr(
                    &mut attributes,
                    BAND_ATTR_HT_AMPDU_FACTOR,
                    &[ht.ampdu_parameters & 0x03],
                );
                push_attr(
                    &mut attributes,
                    BAND_ATTR_HT_AMPDU_DENSITY,
                    &[(ht.ampdu_parameters >> 2) & 0x07],
                );
            }
            if !is_2ghz {
                if let Some(vht) = interface.phy_capabilities.vht {
                    push_attr(&mut attributes, BAND_ATTR_VHT_MCS_SET, &vht.mcs_set);
                    push_attr(
                        &mut attributes,
                        BAND_ATTR_VHT_CAPA,
                        &vht.capability.to_ne_bytes(),
                    );
                }
            }
            push_attr(&mut bands, band_id | NLA_F_NESTED, &attributes);
        }
    }
    if !bands.is_empty() {
        push_attr(&mut payload, ATTR_WIPHY_BANDS | NLA_F_NESTED, &bands);
    }
    nl80211_message(request, port_id, FAMILY_ID, payload, multipart)
}

fn append_supported_rates(band_attributes: &mut Vec<u8>, is_2ghz: bool) {
    const RATES_2GHZ_100KBPS: [u16; 12] = [10, 20, 55, 110, 60, 90, 120, 180, 240, 360, 480, 540];
    const RATES_5GHZ_100KBPS: [u16; 8] = [60, 90, 120, 180, 240, 360, 480, 540];
    let rates = if is_2ghz {
        &RATES_2GHZ_100KBPS[..]
    } else {
        &RATES_5GHZ_100KBPS[..]
    };
    let mut nested = Vec::new();
    for (index, rate) in rates.iter().enumerate() {
        let mut attributes = Vec::new();
        push_attr(&mut attributes, BITRATE_ATTR_RATE, &rate.to_ne_bytes());
        if is_2ghz && index < 4 {
            push_attr(&mut attributes, BITRATE_ATTR_2GHZ_SHORTPREAMBLE, &[]);
        }
        push_attr(&mut nested, (index + 1) as u16, &attributes);
    }
    push_attr(band_attributes, BAND_ATTR_RATES | NLA_F_NESTED, &nested);
}

fn nl80211_message(
    request: &NlMsgHdr,
    port_id: u32,
    family: u16,
    mut payload: Vec<u8>,
    multipart: bool,
) -> Vec<u8> {
    let header_len = size_of::<NlMsgHdr>();
    let mut message = vec![0; header_len];
    message.append(&mut payload);
    let message_len = message.len() as u32;
    write_struct(
        &mut message[..header_len],
        &NlMsgHdr {
            nlmsg_len: message_len,
            nlmsg_type: family,
            nlmsg_flags: if multipart { NLM_F_MULTI } else { 0 },
            nlmsg_seq: request.nlmsg_seq,
            nlmsg_pid: port_id,
        },
    );
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uapi_command_and_attribute_ids_match_the_header() {
        assert_eq!(CMD_GET_WIPHY, 1);
        assert_eq!(CMD_GET_INTERFACE, 5);
        assert_eq!(CMD_GET_SCAN, 32);
        assert_eq!(CMD_TRIGGER_SCAN, 33);
        assert_eq!(CMD_NEW_SCAN_RESULTS, 34);
        assert_eq!(CMD_SCAN_ABORTED, 35);
        assert_eq!(CMD_ABORT_SCAN, 114);
        assert_eq!(CMD_GET_REG, 31);
        assert_eq!(CMD_CONNECT, 46);
        assert_eq!(CMD_DISCONNECT, 48);
        assert_eq!(CMD_GET_STATION, 17);
        assert_eq!(CMD_NEW_STATION, 19);
        assert_eq!(CMD_GET_KEY, 9);
        assert_eq!(CMD_SET_KEY, 10);
        assert_eq!(CMD_NEW_KEY, 11);
        assert_eq!(CMD_DEL_KEY, 12);
        assert_eq!(CMD_SET_PMKSA, 52);
        assert_eq!(CMD_DEL_PMKSA, 53);
        assert_eq!(CMD_FLUSH_PMKSA, 54);
        assert_eq!(ATTR_WIPHY_FREQ, 38);
        assert_eq!(ATTR_STATUS_CODE, 72);
        assert_eq!(ATTR_REQ_IE, 77);
        assert_eq!(ATTR_RESP_IE, 78);
        assert_eq!(ATTR_PMKID, 85);
        assert_eq!(ATTR_REG_ALPHA2, 33);
        assert_eq!(ATTR_IFINDEX, 3);
        assert_eq!(ATTR_IFNAME, 4);
        assert_eq!(ATTR_STA_INFO, 21);
        assert_eq!(MULTICAST_GROUPS[1], "scan");
        assert_eq!(MULTICAST_GROUPS[3], "mlme");
    }

    #[test]
    fn nl80211_family_reply_carries_dynamic_id_version_and_multicast_names() {
        let request = NlMsgHdr {
            nlmsg_len: size_of::<NlMsgHdr>() as u32,
            nlmsg_type: GENL_ID_CTRL,
            nlmsg_flags: 0,
            nlmsg_seq: 7,
            nlmsg_pid: 22,
        };
        let message = family_message(&request, 22);
        assert_eq!(
            u16::from_ne_bytes(message[4..6].try_into().unwrap()),
            GENL_ID_CTRL
        );
        let generic = size_of::<NlMsgHdr>();
        assert_eq!(message[generic], CTRL_CMD_NEWFAMILY);
        let attrs = &message[generic + size_of::<GenlMsgHdr>()..];
        let mut family_id = None;
        let mut family_name = None;
        let mut groups = Vec::new();
        for_each_rtattr(attrs, |kind, value| match kind {
            CTRL_ATTR_FAMILY_ID => {
                family_id = Some(u16::from_ne_bytes(value.try_into().unwrap()));
                Ok(())
            }
            CTRL_ATTR_FAMILY_NAME => {
                family_name = Some(decode_link_name(value)?);
                Ok(())
            }
            CTRL_ATTR_MCAST_GROUPS => {
                for_each_rtattr(value, |group_id, group| {
                    let mut name = None;
                    for_each_rtattr(group, |attribute, bytes| match attribute {
                        CTRL_ATTR_MCAST_GRP_NAME => {
                            name = Some(decode_link_name(bytes)?);
                            Ok(())
                        }
                        _ => Ok(()),
                    })?;
                    groups.push((group_id, name.ok_or(AxError::InvalidInput)?));
                    Ok(())
                })?;
                Ok(())
            }
            _ => Ok(()),
        })
        .unwrap();
        assert_eq!(family_id, Some(FAMILY_ID));
        assert_eq!(family_name.as_deref(), Some(FAMILY_NAME));
        assert_eq!(groups.len(), MULTICAST_GROUPS.len());
        assert_eq!(groups[0].0, 1);
        assert_eq!(groups[0].1, "config");
        assert_eq!(groups[1].0, 2);
        assert_eq!(groups[1].1, "scan");
    }

    #[test]
    fn empty_wiphy_and_interface_dumps_finish_with_multipart_done() {
        let request = NlMsgHdr {
            nlmsg_len: size_of::<NlMsgHdr>() as u32,
            nlmsg_type: FAMILY_ID,
            nlmsg_flags: NLM_F_DUMP,
            nlmsg_seq: 0x1234,
            nlmsg_pid: 9,
        };
        let response = empty_dump_response(&request, 9);
        let header = read_unaligned::<NlMsgHdr>(&response).unwrap();
        assert_eq!(header.nlmsg_type, NLMSG_DONE);
        assert_eq!(header.nlmsg_seq, request.nlmsg_seq);
        assert_eq!(header.nlmsg_pid, 9);
        assert_ne!(header.nlmsg_flags & NLM_F_MULTI, 0);
        assert_eq!(response.len(), size_of::<NlMsgHdr>() + size_of::<i32>());
    }

    #[test]
    fn regulatory_get_encodes_the_global_world_alpha2_attribute() {
        let request = NlMsgHdr {
            nlmsg_len: size_of::<NlMsgHdr>() as u32,
            nlmsg_type: FAMILY_ID,
            nlmsg_flags: 0,
            nlmsg_seq: 2,
            nlmsg_pid: 3,
        };
        let message = regulatory_message(&request, 3);
        let generic = size_of::<NlMsgHdr>();
        assert_eq!(message[generic], CMD_GET_REG);
        let attrs = &message[generic + size_of::<GenlMsgHdr>()..];
        let mut alpha2 = None;
        for_each_rtattr(attrs, |kind, value| {
            if kind == ATTR_REG_ALPHA2 {
                alpha2 = Some(decode_link_name(value)?);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(alpha2.as_deref(), Some("00"));
    }

    #[test]
    fn registered_interface_and_wiphy_records_match_uapi_attribute_layout() {
        let request = NlMsgHdr {
            nlmsg_len: size_of::<NlMsgHdr>() as u32,
            nlmsg_type: FAMILY_ID,
            nlmsg_flags: 0,
            nlmsg_seq: 4,
            nlmsg_pid: 8,
        };
        let interface = axnet::WirelessInterfaceInfo {
            name: "wlan0".into(),
            ifindex: 9,
            phy_index: 2,
            rfkill_index: 2,
            mac_address: [2, 0, 0, 0, 0, 9],
            frequencies: alloc::vec![
                axnet::WirelessFrequencyInfo {
                    frequency_mhz: 2412,
                    no_ir: false,
                },
                axnet::WirelessFrequencyInfo {
                    frequency_mhz: 5180,
                    no_ir: true,
                },
            ],
            phy_capabilities: axnet::WirelessPhyCapabilities {
                ht: Some(axnet::WirelessHtCapabilities {
                    capability: 0x016e,
                    ampdu_parameters: 0x17,
                    mcs_set: [0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
                }),
                vht: Some(axnet::WirelessVhtCapabilities {
                    capability: 0x0380_01e4,
                    mcs_set: [0xfa, 0xff, 0, 0, 0xfa, 0xff, 0, 0],
                }),
            },
            soft_blocked: false,
            hard_blocked: false,
        };
        let message = interface_message(&request, 8, &interface, false);
        let header = read_unaligned::<NlMsgHdr>(&message).unwrap();
        assert_eq!(header.nlmsg_type, FAMILY_ID);
        assert_eq!(header.nlmsg_flags, 0);
        let generic = size_of::<NlMsgHdr>();
        assert_eq!(message[generic], CMD_NEW_INTERFACE);
        let attrs = &message[generic + size_of::<GenlMsgHdr>()..];
        let mut ifindex = None;
        let mut ifname = None;
        let mut iftype = None;
        for_each_rtattr(attrs, |kind, value| match kind {
            ATTR_IFINDEX => {
                ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                Ok(())
            }
            ATTR_IFNAME => {
                ifname = Some(decode_link_name(value)?);
                Ok(())
            }
            ATTR_IFTYPE => {
                iftype = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                Ok(())
            }
            _ => Ok(()),
        })
        .unwrap();
        assert_eq!(ifindex, Some(9));
        assert_eq!(ifname.as_deref(), Some("wlan0"));
        assert_eq!(iftype, Some(IFTYPE_STATION));

        let message = wiphy_message(&request, 8, &interface, true);
        let generic = size_of::<NlMsgHdr>();
        assert_eq!(message[generic], CMD_NEW_WIPHY);
        assert_ne!(
            read_unaligned::<NlMsgHdr>(&message).unwrap().nlmsg_flags & NLM_F_MULTI,
            0
        );
        let attrs = &message[generic + size_of::<GenlMsgHdr>()..];
        let mut frequencies = Vec::new();
        let mut band_rates = Vec::new();
        let mut band_capabilities = Vec::new();
        for_each_rtattr(attrs, |kind, value| {
            if kind == ATTR_WIPHY_BANDS {
                for_each_rtattr(value, |band, band_attributes| {
                    let mut ht_capability = None;
                    let mut ht_mcs = None;
                    let mut ampdu_factor = None;
                    let mut ampdu_density = None;
                    let mut vht_capability = None;
                    let mut vht_mcs = None;
                    for_each_rtattr(band_attributes, |band_kind, band_value| {
                        if band_kind == BAND_ATTR_FREQS {
                            for_each_rtattr(band_value, |_, frequency_attributes| {
                                let mut mhz = None;
                                let mut no_ir = false;
                                for_each_rtattr(frequency_attributes, |frequency_kind, bytes| {
                                    match frequency_kind {
                                        FREQ_ATTR_FREQ => {
                                            mhz =
                                                Some(u32::from_ne_bytes(bytes.try_into().unwrap()));
                                        }
                                        FREQ_ATTR_NO_IR => no_ir = true,
                                        _ => {}
                                    }
                                    Ok(())
                                })?;
                                frequencies.push((band, mhz.ok_or(AxError::InvalidInput)?, no_ir));
                                Ok(())
                            })?;
                        } else if band_kind == BAND_ATTR_RATES {
                            let mut rates = Vec::new();
                            for_each_rtattr(band_value, |_, rate_attributes| {
                                for_each_rtattr(rate_attributes, |rate_kind, bytes| {
                                    if rate_kind == BITRATE_ATTR_RATE {
                                        rates.push(u16::from_ne_bytes(bytes.try_into().unwrap()));
                                    }
                                    Ok(())
                                })
                            })?;
                            band_rates.push((band, rates));
                        } else if band_kind == BAND_ATTR_HT_CAPA {
                            ht_capability =
                                Some(u16::from_ne_bytes(band_value.try_into().unwrap()));
                        } else if band_kind == BAND_ATTR_HT_MCS_SET {
                            ht_mcs = Some(band_value.to_vec());
                        } else if band_kind == BAND_ATTR_HT_AMPDU_FACTOR {
                            ampdu_factor = Some(band_value[0]);
                        } else if band_kind == BAND_ATTR_HT_AMPDU_DENSITY {
                            ampdu_density = Some(band_value[0]);
                        } else if band_kind == BAND_ATTR_VHT_CAPA {
                            vht_capability =
                                Some(u32::from_ne_bytes(band_value.try_into().unwrap()));
                        } else if band_kind == BAND_ATTR_VHT_MCS_SET {
                            vht_mcs = Some(band_value.to_vec());
                        }
                        Ok(())
                    })?;
                    band_capabilities.push((
                        band,
                        ht_capability,
                        ht_mcs,
                        ampdu_factor,
                        ampdu_density,
                        vht_capability,
                        vht_mcs,
                    ));
                    Ok(())
                })?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(frequencies, [(0, 2412, false), (1, 5180, true)]);
        assert_eq!(
            band_rates,
            [
                (
                    0,
                    vec![10, 20, 55, 110, 60, 90, 120, 180, 240, 360, 480, 540]
                ),
                (1, vec![60, 90, 120, 180, 240, 360, 480, 540]),
            ]
        );
        assert_eq!(band_capabilities[0].0, 0);
        assert_eq!(band_capabilities[0].1, Some(0x016e));
        assert_eq!(band_capabilities[0].2.as_ref().unwrap().len(), 16);
        assert_eq!(band_capabilities[0].3, Some(3));
        assert_eq!(band_capabilities[0].4, Some(5));
        assert_eq!(band_capabilities[0].5, None);
        assert_eq!(band_capabilities[1].0, 1);
        assert_eq!(band_capabilities[1].5, Some(0x0380_01e4));
        assert_eq!(
            band_capabilities[1].6.as_ref().unwrap(),
            &[0xfa, 0xff, 0, 0, 0xfa, 0xff, 0, 0]
        );
        let mut max_ssids = None;
        let mut cipher_suites = Vec::new();
        let mut max_pmkids = None;
        let mut commands = Vec::new();
        for_each_rtattr(attrs, |kind, value| {
            if kind == ATTR_MAX_NUM_SCAN_SSIDS {
                max_ssids = value.first().copied();
            } else if kind == ATTR_CIPHER_SUITES {
                if !value.len().is_multiple_of(4) {
                    return Err(AxError::InvalidInput);
                }
                for cipher in value.chunks_exact(4) {
                    cipher_suites.push(u32::from_ne_bytes(cipher.try_into().unwrap()));
                }
            } else if kind == ATTR_MAX_NUM_PMKIDS {
                max_pmkids = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            } else if kind == ATTR_SUPPORTED_COMMANDS {
                for_each_rtattr(value, |_, command| {
                    commands.push(u32::from_ne_bytes(command.try_into().unwrap()));
                    Ok(())
                })?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(max_ssids, Some(1));
        assert_eq!(cipher_suites, [0x000fac04]);
        assert_eq!(max_pmkids, Some(0));
        assert_eq!(commands, [1, 5, 33, 114, 32, 31, 46, 48, 17, 9, 10, 11, 12]);
    }

    #[test]
    fn selectors_reject_truncation_duplicates_and_malformed_names() {
        let mut attrs = Vec::new();
        push_attr(&mut attrs, ATTR_IFINDEX, &7u32.to_ne_bytes());
        assert_eq!(parse_selectors(&attrs).unwrap().ifindex, Some(7));
        push_attr(&mut attrs, ATTR_IFINDEX, &8u32.to_ne_bytes());
        assert_eq!(parse_selectors(&attrs), Err(AxError::InvalidInput));
        let mut bad_name = Vec::new();
        push_attr(&mut bad_name, ATTR_IFNAME, b"wlan0");
        assert_eq!(parse_selectors(&bad_name), Err(AxError::InvalidInput));
    }

    #[test]
    fn trigger_scan_parses_nested_ssid_and_frequency_attributes() {
        let mut ssids = Vec::new();
        push_attr(&mut ssids, 1, b"test-net");
        let mut frequencies = Vec::new();
        push_attr(&mut frequencies, 1, &2412u32.to_ne_bytes());
        push_attr(&mut frequencies, 2, &5180u32.to_ne_bytes());
        let mut attrs = Vec::new();
        push_attr(&mut attrs, ATTR_IFINDEX, &9u32.to_ne_bytes());
        push_attr(&mut attrs, ATTR_SCAN_SSIDS | NLA_F_NESTED, &ssids);
        push_attr(
            &mut attrs,
            ATTR_SCAN_FREQUENCIES | NLA_F_NESTED,
            &frequencies,
        );
        let (ifindex, request) = parse_scan_request(&attrs).unwrap();
        assert_eq!(ifindex, 9);
        assert_eq!(request.ssid, b"test-net");
        assert_eq!(request.frequencies_mhz, [2412, 5180]);

        push_attr(&mut attrs, ATTR_IFINDEX, &10u32.to_ne_bytes());
        assert_eq!(parse_scan_request(&attrs), Err(AxError::InvalidInput));
    }

    #[test]
    fn connect_and_disconnect_parse_station_uapi_attributes() {
        let mut suites = Vec::new();
        push_attr(&mut suites, 1, &0x000fac04u32.to_ne_bytes());
        push_attr(&mut suites, 2, &0x000fac02u32.to_ne_bytes());
        let mut akms = Vec::new();
        push_attr(&mut akms, 1, &0x000fac02u32.to_ne_bytes());
        let mut attrs = Vec::new();
        push_attr(&mut attrs, ATTR_IFINDEX, &12u32.to_ne_bytes());
        push_attr(&mut attrs, ATTR_SSID, b"secure");
        push_attr(&mut attrs, ATTR_MAC, &[2, 1, 2, 3, 4, 5]);
        push_attr(&mut attrs, ATTR_AUTH_TYPE, &0u32.to_ne_bytes());
        push_attr(&mut attrs, ATTR_WPA_VERSIONS, &2u32.to_ne_bytes());
        push_attr(
            &mut attrs,
            ATTR_CIPHER_SUITES_PAIRWISE | NLA_F_NESTED,
            &suites,
        );
        push_attr(
            &mut attrs,
            ATTR_CIPHER_SUITE_GROUP,
            &0x000fac04u32.to_ne_bytes(),
        );
        push_attr(&mut attrs, ATTR_AKM_SUITES | NLA_F_NESTED, &akms);
        push_attr(&mut attrs, ATTR_CONNECT_IE, &[48, 2, 1, 0]);
        let (ifindex, connect) = parse_connect_request(&attrs).unwrap();
        assert_eq!(ifindex, 12);
        assert_eq!(connect.ssid, b"secure");
        assert_eq!(connect.bssid, Some([2, 1, 2, 3, 4, 5]));
        assert_eq!(connect.authentication_type, 0);
        assert_eq!(connect.wpa_versions, 2);
        assert_eq!(connect.pairwise_ciphers, [0x000fac04, 0x000fac02]);
        assert_eq!(connect.group_cipher, Some(0x000fac04));
        assert_eq!(connect.akm_suites, [0x000fac02]);
        assert_eq!(connect.information_elements, [48, 2, 1, 0]);

        let mut disconnect = Vec::new();
        push_attr(&mut disconnect, ATTR_IFINDEX, &12u32.to_ne_bytes());
        push_attr(&mut disconnect, ATTR_REASON_CODE, &3u16.to_ne_bytes());
        assert_eq!(parse_disconnect_request(&disconnect), Ok((12, 3)));
        push_attr(&mut disconnect, ATTR_REASON_CODE, &4u16.to_ne_bytes());
        assert_eq!(
            parse_disconnect_request(&disconnect),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn station_request_requires_one_ifindex_and_accepts_optional_mac() {
        let mut attrs = Vec::new();
        push_attr(&mut attrs, ATTR_IFINDEX, &12u32.to_ne_bytes());
        push_attr(&mut attrs, ATTR_MAC, &[2, 3, 4, 5, 6, 7]);
        assert_eq!(
            parse_station_request(&attrs),
            Ok((12, Some([2, 3, 4, 5, 6, 7])))
        );
        push_attr(&mut attrs, ATTR_MAC, &[2, 3, 4, 5, 6, 8]);
        assert_eq!(parse_station_request(&attrs), Err(AxError::InvalidInput));
    }

    #[test]
    fn key_requests_encode_standard_ccmp_key_fields_and_reject_duplicates() {
        let mut attrs = Vec::new();
        push_attr(&mut attrs, ATTR_IFINDEX, &12u32.to_ne_bytes());
        push_attr(&mut attrs, ATTR_KEY_IDX, &[1]);
        push_attr(&mut attrs, ATTR_KEY_CIPHER, &0x000fac04u32.to_ne_bytes());
        push_attr(&mut attrs, ATTR_KEY_DATA, &[0x55; 16]);
        push_attr(&mut attrs, ATTR_KEY_SEQ, &[1, 0, 0, 0, 0, 0]);
        push_attr(&mut attrs, ATTR_MAC, &[2, 3, 4, 5, 6, 7]);
        let (ifindex, key) = parse_key_request(&attrs, CMD_NEW_KEY).unwrap();
        assert_eq!(ifindex, 12);
        assert_eq!(key.index, 1);
        assert_eq!(key.cipher_suite, 0x000fac04);
        assert_eq!(key.key_data, [0x55; 16]);
        assert_eq!(key.sequence, [1, 0, 0, 0, 0, 0]);
        assert_eq!(key.peer, Some([2, 3, 4, 5, 6, 7]));
        push_attr(&mut attrs, ATTR_KEY_IDX, &[2]);
        assert_eq!(
            parse_key_request(&attrs, CMD_NEW_KEY),
            Err(AxError::InvalidInput)
        );

        let mut default = Vec::new();
        push_attr(&mut default, ATTR_IFINDEX, &12u32.to_ne_bytes());
        push_attr(&mut default, ATTR_KEY_IDX, &[1]);
        push_attr(&mut default, ATTR_KEY_DEFAULT, &[]);
        let mut default_types = Vec::new();
        push_attr(&mut default_types, KEY_DEFAULT_TYPE_MULTICAST, &[]);
        push_attr(
            &mut default,
            ATTR_KEY_DEFAULT_TYPES | NLA_F_NESTED,
            &default_types,
        );
        let parsed_default = parse_key_request(&default, CMD_SET_KEY).unwrap().1;
        assert_eq!(parsed_default.index, 1);
        assert!(parsed_default.default_multicast);
        assert!(!parsed_default.default_unicast);
    }

    #[test]
    fn supplicant_pmksa_updates_are_validated_but_user_space_owned() {
        let mut set = Vec::new();
        push_attr(&mut set, ATTR_IFINDEX, &12u32.to_ne_bytes());
        push_attr(&mut set, ATTR_MAC, &[2, 3, 4, 5, 6, 7]);
        push_attr(&mut set, ATTR_PMKID, &[0x55; 16]);
        assert_eq!(
            parse_pmksa_request(&set, CMD_SET_PMKSA),
            Ok((12, Some([2, 3, 4, 5, 6, 7]), Some([0x55; 16])))
        );

        let mut flush = Vec::new();
        push_attr(&mut flush, ATTR_IFINDEX, &12u32.to_ne_bytes());
        assert_eq!(
            parse_pmksa_request(&flush, CMD_FLUSH_PMKSA),
            Ok((12, None, None))
        );
        push_attr(&mut flush, ATTR_PMKID, &[0; 8]);
        assert_eq!(
            parse_pmksa_request(&flush, CMD_FLUSH_PMKSA),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn get_key_reply_returns_only_the_cipher_and_packet_number() {
        let request = NlMsgHdr {
            nlmsg_len: size_of::<NlMsgHdr>() as u32,
            nlmsg_type: FAMILY_ID,
            nlmsg_flags: 0,
            nlmsg_seq: 7,
            nlmsg_pid: 8,
        };
        let key = axnet::WirelessKeyConfig {
            index: 1,
            peer: Some([2, 3, 4, 5, 6, 7]),
            ..Default::default()
        };
        let info = axnet::WirelessKeyInfo {
            cipher_suite: 0x000fac04,
            sequence: [1, 2, 3, 4, 5, 6].to_vec(),
        };
        let message = key_message(&request, 8, 12, &key, &info);
        let generic = size_of::<NlMsgHdr>();
        assert_eq!(message[generic], CMD_NEW_KEY);
        let mut saw_data = false;
        let mut sequence = None;
        for_each_rtattr(
            &message[generic + size_of::<GenlMsgHdr>()..],
            |kind, value| {
                if kind == ATTR_KEY_DATA {
                    saw_data = true;
                } else if kind == ATTR_KEY_SEQ {
                    sequence = Some(value.to_vec());
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(!saw_data);
        assert_eq!(sequence, Some(info.sequence));
    }

    #[test]
    fn station_dump_encodes_bssid_and_signed_signal_as_nested_sta_info() {
        let request = NlMsgHdr {
            nlmsg_len: size_of::<NlMsgHdr>() as u32,
            nlmsg_type: FAMILY_ID,
            nlmsg_flags: NLM_F_DUMP,
            nlmsg_seq: 3,
            nlmsg_pid: 9,
        };
        let interface = axnet::WirelessInterfaceInfo {
            name: "wlan0".into(),
            ifindex: 12,
            phy_index: 0,
            rfkill_index: 0,
            mac_address: [2, 1, 2, 3, 4, 5],
            frequencies: Vec::new(),
            phy_capabilities: Default::default(),
            soft_blocked: false,
            hard_blocked: false,
        };
        let station = axnet::WirelessStationInfo {
            bssid: [2, 3, 4, 5, 6, 7],
            frequency_mhz: 2437,
            signal_mbm: -6123,
            association_id: 17,
            request_ies: Vec::new(),
            response_ies: Vec::new(),
        };
        let message = station_message(&request, 9, &interface, &station, true);
        let generic = size_of::<NlMsgHdr>();
        assert_eq!(message[generic], CMD_NEW_STATION);
        let mut attrs = Vec::new();
        for_each_rtattr(
            &message[generic + size_of::<GenlMsgHdr>()..],
            |kind, value| {
                attrs.push((kind, value.to_vec()));
                Ok(())
            },
        )
        .unwrap();
        assert!(
            attrs
                .iter()
                .any(|(kind, value)| *kind == ATTR_MAC && value == &station.bssid)
        );
        let nested = attrs
            .iter()
            .find(|(kind, _)| *kind == (ATTR_STA_INFO | NLA_F_NESTED))
            .unwrap();
        let mut signal = None;
        for_each_rtattr(&nested.1, |kind, value| {
            if kind == STA_INFO_SIGNAL {
                signal = Some(value[0] as i8);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(signal, Some(-61));
    }

    #[test]
    fn successful_connect_event_has_standard_mlme_attributes() {
        let station = axnet::WirelessStationInfo {
            bssid: [2, 3, 4, 5, 6, 7],
            frequency_mhz: 5180,
            signal_mbm: -4500,
            association_id: 17,
            request_ies: vec![0, 1, b'a'],
            response_ies: vec![1, 1, 2],
        };
        let message = connect_event_message(12, &station);
        let generic = size_of::<NlMsgHdr>();
        assert_eq!(message[generic], CMD_CONNECT);
        let mut status = None;
        let mut frequency = None;
        let mut request_ies = None;
        let mut response_ies = None;
        for_each_rtattr(
            &message[generic + size_of::<GenlMsgHdr>()..],
            |kind, value| {
                if kind == ATTR_STATUS_CODE {
                    status = Some(u16::from_ne_bytes(value.try_into().unwrap()));
                } else if kind == ATTR_WIPHY_FREQ {
                    frequency = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                } else if kind == ATTR_REQ_IE {
                    request_ies = Some(value.to_vec());
                } else if kind == ATTR_RESP_IE {
                    response_ies = Some(value.to_vec());
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(status, Some(0));
        assert_eq!(frequency, Some(5180));
        assert_eq!(request_ies, Some(station.request_ies));
        assert_eq!(response_ies, Some(station.response_ies));
    }

    #[test]
    fn disconnect_event_has_interface_peer_and_reason() {
        let message = disconnect_event_message(12, [2, 3, 4, 5, 6, 7], 3);
        let generic = size_of::<NlMsgHdr>();
        assert_eq!(message[generic], CMD_DISCONNECT);
        let mut ifindex = None;
        let mut address = None;
        let mut reason = None;
        for_each_rtattr(
            &message[generic + size_of::<GenlMsgHdr>()..],
            |kind, value| {
                match kind {
                    ATTR_IFINDEX => ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap())),
                    ATTR_MAC => address = Some(value.try_into().unwrap()),
                    ATTR_REASON_CODE => {
                        reason = Some(u16::from_ne_bytes(value.try_into().unwrap()))
                    }
                    _ => {}
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(ifindex, Some(12));
        assert_eq!(address, Some([2, 3, 4, 5, 6, 7]));
        assert_eq!(reason, Some(3));
    }

    #[test]
    fn scan_result_message_uses_nested_bss_uapi_fields() {
        let request = NlMsgHdr {
            nlmsg_len: size_of::<NlMsgHdr>() as u32,
            nlmsg_type: FAMILY_ID,
            nlmsg_flags: NLM_F_DUMP,
            nlmsg_seq: 8,
            nlmsg_pid: 22,
        };
        let bss = axnet::WirelessBssInfo {
            bssid: [2, 3, 4, 5, 6, 7],
            frequency_mhz: 2412,
            signal_mbm: -4200,
            timestamp: 1234,
            beacon_interval: 100,
            capability: 0x0431,
            information_elements: [0, 3, b'a', b'p', b'1'].to_vec(),
            is_probe_response: false,
        };
        let message = scan_bss_message(&request, 22, 9, &bss);
        let attrs = &message[size_of::<NlMsgHdr>() + size_of::<GenlMsgHdr>()..];
        let mut ifindex = None;
        let mut nested_bss = None;
        for_each_rtattr(attrs, |kind, value| {
            if kind == ATTR_IFINDEX {
                ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap()));
            } else if kind == ATTR_BSS {
                nested_bss = Some(value.to_vec());
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(ifindex, Some(9));
        let mut decoded = Vec::new();
        for_each_rtattr(nested_bss.as_deref().unwrap(), |kind, value| {
            decoded.push((kind, value.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(decoded[0], (BSS_ATTR_BSSID, bss.bssid.to_vec()));
        assert_eq!(
            decoded[1],
            (BSS_ATTR_FREQUENCY, 2412u32.to_ne_bytes().to_vec())
        );
        assert_eq!(decoded[2], (BSS_ATTR_TSF, 1234u64.to_ne_bytes().to_vec()));
        assert_eq!(
            decoded[6],
            (BSS_ATTR_SIGNAL_MBM, (-4200i32).to_ne_bytes().to_vec())
        );
    }

    #[test]
    fn scan_completion_multicast_event_uses_scan_group_commands_and_ifindex() {
        for (event, command) in [
            (axnet::WirelessScanEvent::Results, CMD_NEW_SCAN_RESULTS),
            (axnet::WirelessScanEvent::Aborted, CMD_SCAN_ABORTED),
        ] {
            let message = scan_event_message(17, event);
            let header = read_unaligned::<NlMsgHdr>(&message).unwrap();
            assert_eq!(header.nlmsg_type, FAMILY_ID);
            assert_eq!(header.nlmsg_seq, 0);
            let generic = read_unaligned::<GenlMsgHdr>(&message[size_of::<NlMsgHdr>()..]).unwrap();
            assert_eq!(generic.cmd, command);
            let mut ifindex = None;
            for_each_rtattr(
                &message[size_of::<NlMsgHdr>() + size_of::<GenlMsgHdr>()..],
                |kind, value| {
                    if kind == ATTR_IFINDEX {
                        ifindex = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(ifindex, Some(17));
        }
        assert_eq!(NL80211_SCAN_GROUP_MASK, 2);
    }
}
