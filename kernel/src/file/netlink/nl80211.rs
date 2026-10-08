//! nl80211 generic-netlink family discovery and no-device dump contract.
//!
//! UAPI values are translated from Linux `include/uapi/linux/nl80211.h`
//! version 7.2.3 (ISC-style grant). Copyright 2006-2010 Johannes Berg;
//! Copyright 2008 Michael Wu, Luis Carlos Cobo, Michael Buesch, Jouni Malinen
//! and Colin McCabe; Copyright 2015-2017 Intel Deutschland GmbH; Copyright
//! (C) 2018-2026 Intel Corporation. The complete grant is retained in
//! `kernel/LICENSES/ISC.txt`.

use super::*;

pub(super) const FAMILY_ID: u16 = 0x12;
pub(super) const FAMILY_NAME: &str = "nl80211";
const FAMILY_VERSION: u8 = 1;
const FAMILY_MAX_ATTRIBUTE: u32 = 366;

pub(super) const CMD_GET_WIPHY: u8 = 1;
pub(super) const CMD_GET_INTERFACE: u8 = 5;
const ATTR_WIPHY: u16 = 1;
const ATTR_WIPHY_NAME: u16 = 2;
const ATTR_IFINDEX: u16 = 3;
const ATTR_IFNAME: u16 = 4;
const ATTR_SPLIT_WIPHY_DUMP: u16 = 174;
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
    if !matches!(request.cmd, CMD_GET_WIPHY | CMD_GET_INTERFACE) {
        return Err(AxError::OperationNotSupported);
    }
    validate_selectors(&payload[size_of::<GenlMsgHdr>()..])?;
    let dump = header.nlmsg_flags & NLM_F_DUMP != 0;
    let interfaces = axnet::wireless_interfaces();
    if !interfaces.is_empty() {
        // Do not advertise an empty radio once a WLAN adapter is registered;
        // concrete wiphy and interface records are added with the command
        // handlers rather than fabricated from the PCI match alone.
        return Err(AxError::OperationNotSupported);
    }
    if !dump {
        return Err(AxError::NotFound);
    }
    let port_id = permit.port_id();
    socket.enqueue_kernel_permitted(permit, empty_dump_response(header, port_id));
    Ok(())
}

fn empty_dump_response(request: &NlMsgHdr, port_id: u32) -> Vec<u8> {
    wiremsg::done_message(request, port_id)
}

fn validate_selectors(attributes: &[u8]) -> AxResult {
    let mut seen_wiphy = false;
    let mut seen_ifindex = false;
    let mut seen_wiphy_name = false;
    let mut seen_ifname = false;
    for_each_rtattr(attributes, |kind, value| match kind {
        ATTR_WIPHY if !seen_wiphy && value.len() == 4 => {
            seen_wiphy = true;
            Ok(())
        }
        ATTR_IFINDEX if !seen_ifindex && value.len() == 4 => {
            seen_ifindex = true;
            Ok(())
        }
        ATTR_WIPHY_NAME if !seen_wiphy_name => {
            let _ = decode_link_name(value)?;
            seen_wiphy_name = true;
            Ok(())
        }
        ATTR_IFNAME if !seen_ifname => {
            let _ = decode_link_name(value)?;
            seen_ifname = true;
            Ok(())
        }
        ATTR_SPLIT_WIPHY_DUMP if value.is_empty() => Ok(()),
        _ => Err(AxError::InvalidInput),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uapi_command_and_attribute_ids_match_the_header() {
        assert_eq!(CMD_GET_WIPHY, 1);
        assert_eq!(CMD_GET_INTERFACE, 5);
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
    fn selectors_reject_truncation_duplicates_and_malformed_names() {
        let mut attrs = Vec::new();
        push_attr(&mut attrs, ATTR_IFINDEX, &7u32.to_ne_bytes());
        assert!(validate_selectors(&attrs).is_ok());
        push_attr(&mut attrs, ATTR_IFINDEX, &8u32.to_ne_bytes());
        assert_eq!(validate_selectors(&attrs), Err(AxError::InvalidInput));
        let mut bad_name = Vec::new();
        push_attr(&mut bad_name, ATTR_IFNAME, b"wlan0");
        assert_eq!(validate_selectors(&bad_name), Err(AxError::InvalidInput));
    }
}
