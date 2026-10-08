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
const CMD_GET_REG: u8 = 31;
const ATTR_WIPHY: u16 = 1;
const ATTR_WIPHY_NAME: u16 = 2;
const ATTR_IFINDEX: u16 = 3;
const ATTR_IFNAME: u16 = 4;
const ATTR_IFTYPE: u16 = 5;
const ATTR_MAC: u16 = 6;
const ATTR_WIPHY_BANDS: u16 = 22;
const ATTR_SUPPORTED_IFTYPES: u16 = 32;
const ATTR_SPLIT_WIPHY_DUMP: u16 = 174;
const ATTR_REG_ALPHA2: u16 = 33;
const BAND_ATTR_FREQS: u16 = 1;
const BAND_ATTR_RATES: u16 = 2;
const FREQ_ATTR_FREQ: u16 = 1;
const FREQ_ATTR_NO_IR: u16 = 3;
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
        CMD_GET_WIPHY | CMD_GET_INTERFACE | CMD_GET_SCAN | CMD_GET_REG
    ) {
        return Err(AxError::OperationNotSupported);
    }
    let selectors = parse_selectors(&payload[size_of::<GenlMsgHdr>()..])?;
    let dump = header.nlmsg_flags & NLM_F_DUMP != 0;
    let interfaces = axnet::wireless_interfaces();
    let port_id = permit.port_id();
    let mut records = Vec::new();
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
            if !interfaces.iter().any(|interface| {
                selectors
                    .ifindex
                    .is_none_or(|ifindex| interface.ifindex == ifindex)
                    && selectors
                        .ifname
                        .as_deref()
                        .is_none_or(|name| interface.name == name)
            }) {
                return Err(AxError::NotFound);
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
        assert_eq!(CMD_GET_REG, 31);
        assert_eq!(ATTR_REG_ALPHA2, 33);
        assert_eq!(ATTR_IFINDEX, 3);
        assert_eq!(ATTR_IFNAME, 4);
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
        for_each_rtattr(attrs, |kind, value| {
            if kind == ATTR_WIPHY_BANDS {
                for_each_rtattr(value, |band, band_attributes| {
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
                        }
                        Ok(())
                    })
                })?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(frequencies, [(0, 2412, false), (1, 5180, true)]);
        assert_eq!(
            band_rates,
            [
                (0, vec![10, 20, 55, 110, 60, 90, 120, 180, 240, 360, 480, 540]),
                (1, vec![60, 90, 120, 180, 240, 360, 480, 540]),
            ]
        );
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
}
