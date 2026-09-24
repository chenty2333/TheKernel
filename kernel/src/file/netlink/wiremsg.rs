//! `file::netlink` subsections; see the parent `mod.rs` for the module map.

use super::*;

pub(crate) fn netlink_ack(request: &NlMsgHdr, port_id: u32, error: i32) -> Vec<u8> {
    let header_len = size_of::<NlMsgHdr>();
    let err_len = size_of::<NlMsgErr>();
    let mut out = vec![0; header_len + err_len];
    let hdr = NlMsgHdr {
        nlmsg_len: out.len() as u32,
        nlmsg_type: NLMSG_ERROR,
        nlmsg_flags: 0,
        nlmsg_seq: request.nlmsg_seq,
        nlmsg_pid: port_id,
    };
    write_struct(&mut out[..header_len], &hdr);
    let err = NlMsgErr {
        error,
        msg: *request,
    };
    write_struct(&mut out[header_len..], &err);
    out
}

pub(crate) fn done_message(request: &NlMsgHdr, port_id: u32) -> Vec<u8> {
    let mut out = vec![0; size_of::<NlMsgHdr>()];
    let hdr = NlMsgHdr {
        nlmsg_len: out.len() as u32,
        nlmsg_type: NLMSG_DONE,
        nlmsg_flags: NLM_F_MULTI,
        nlmsg_seq: request.nlmsg_seq,
        nlmsg_pid: port_id,
    };
    write_struct(&mut out, &hdr);
    out
}

fn netlink_message(
    request: &NlMsgHdr,
    port_id: u32,
    msg_type: u16,
    mut payload: Vec<u8>,
) -> Vec<u8> {
    let header_len = size_of::<NlMsgHdr>();
    let mut out = vec![0; header_len];
    out.append(&mut payload);
    let hdr = NlMsgHdr {
        nlmsg_len: out.len() as u32,
        nlmsg_type: msg_type,
        nlmsg_flags: NLM_F_MULTI,
        nlmsg_seq: request.nlmsg_seq,
        nlmsg_pid: port_id,
    };
    write_struct(&mut out[..header_len], &hdr);
    out
}

pub(crate) fn sock_diag_message(
    request: &NlMsgHdr,
    port_id: u32,
    entry: &SocketDiagRegistration,
    extensions: u8,
) -> Vec<u8> {
    // `inet_diag_msg`: family/state/timer/retrans, inet_diag_sockid, then
    // expires/rqueue/wqueue/uid/inode.  Addresses and queues are zero until
    // the transport exposes its bind/connect snapshot; identity, protocol
    // selection and lifecycle are nevertheless the actual live OFD record.
    let mut payload = vec![0_u8; 72];
    payload[0] = entry.family as u8;
    payload[1] = entry.diag_state();
    payload[44..48].copy_from_slice(&(entry.cookie as u32).to_ne_bytes());
    payload[48..52].copy_from_slice(&((entry.cookie >> 32) as u32).to_ne_bytes());
    // No provider extension is invented yet; retaining the parsed extension
    // mask makes the request path complete without changing base selection.
    let _ = extensions;
    netlink_message(request, port_id, SOCK_DIAG_BY_FAMILY, payload)
}

pub(crate) fn generic_family_message(request: &NlMsgHdr, port_id: u32) -> Vec<u8> {
    let mut payload = payload_with(&GenlMsgHdr {
        cmd: CTRL_CMD_NEWFAMILY,
        version: 2,
        reserved: 0,
    });
    push_attr(
        &mut payload,
        CTRL_ATTR_FAMILY_ID,
        &THEKERNEL_GENL_FAMILY_ID.to_ne_bytes(),
    );
    push_attr_string(
        &mut payload,
        CTRL_ATTR_FAMILY_NAME,
        THEKERNEL_GENL_FAMILY_NAME,
    );
    push_attr(&mut payload, CTRL_ATTR_VERSION, &[1, 0, 0, 0]);
    push_attr(&mut payload, CTRL_ATTR_HDRSIZE, &[0, 0, 0, 0]);
    push_attr(&mut payload, CTRL_ATTR_MAXATTR, &[0, 0, 0, 0]);
    netlink_message(request, port_id, GENL_ID_CTRL, payload)
}

fn nft_payload() -> Vec<u8> {
    vec![0, 0, 0, 0]
} // struct nfgenmsg
fn nft_message(request: &NlMsgHdr, port_id: u32, command: u16, payload: Vec<u8>) -> Vec<u8> {
    netlink_message(
        request,
        port_id,
        (NFNL_SUBSYS_NFTABLES << 8) | command,
        payload,
    )
}
pub(crate) fn nft_table_message(request: &NlMsgHdr, port_id: u32, table: &str) -> Vec<u8> {
    let mut payload = nft_payload();
    push_attr_string(&mut payload, NFTA_TABLE_NAME, table);
    nft_message(request, port_id, NFT_MSG_NEWTABLE, payload)
}
pub(crate) fn nft_chain_message(request: &NlMsgHdr, port_id: u32, chain: &NftChain) -> Vec<u8> {
    let mut payload = nft_payload();
    push_attr_string(&mut payload, NFTA_CHAIN_TABLE, &chain.table);
    push_attr_string(&mut payload, NFTA_CHAIN_NAME, &chain.name);
    nft_message(request, port_id, NFT_MSG_NEWCHAIN, payload)
}
pub(crate) fn nft_rule_message(request: &NlMsgHdr, port_id: u32, rule: &NftRule) -> Vec<u8> {
    let mut payload = nft_payload();
    push_attr_string(&mut payload, NFTA_RULE_TABLE, &rule.table);
    push_attr_string(&mut payload, NFTA_RULE_CHAIN, &rule.chain);
    push_attr(&mut payload, NFTA_RULE_HANDLE, &rule.handle.to_ne_bytes());
    nft_message(request, port_id, NFT_MSG_NEWRULE, payload)
}
pub(crate) fn nft_set_message(request: &NlMsgHdr, port_id: u32, set: &NftSet) -> Vec<u8> {
    let mut payload = nft_payload();
    push_attr_string(&mut payload, NFTA_SET_TABLE, &set.table);
    push_attr_string(&mut payload, NFTA_SET_NAME, &set.name);
    push_attr_u32(&mut payload, NFTA_SET_ID, set.id);
    push_attr_u32(&mut payload, NFTA_SET_FLAGS, set.flags);
    push_attr_u32(&mut payload, NFTA_SET_KEY_TYPE, set.key_type);
    push_attr_u32(&mut payload, NFTA_SET_DATA_TYPE, set.data_type);
    nft_message(request, port_id, NFT_MSG_NEWSET, payload)
}
pub(crate) fn nft_element_message(
    request: &NlMsgHdr,
    port_id: u32,
    element: &NftSetElement,
) -> Vec<u8> {
    let mut payload = nft_payload();
    push_attr_string(&mut payload, NFTA_SET_ELEM_LIST_TABLE, &element.table);
    push_attr_string(&mut payload, NFTA_SET_ELEM_LIST_SET, &element.set);
    push_attr(&mut payload, NFTA_SET_ELEM_LIST_ELEMENTS, &element.key);
    nft_message(request, port_id, NFT_MSG_NEWSETELEM, payload)
}

pub(crate) fn payload_with<T: bytemuck::NoUninit>(value: &T) -> Vec<u8> {
    let mut out = vec![0; size_of::<T>()];
    write_struct(&mut out, value);
    out
}

pub(crate) fn push_attr(out: &mut Vec<u8>, attr_type: u16, value: &[u8]) {
    let len = size_of::<RtAttr>() + value.len();
    let aligned = align4(len);
    let start = out.len();
    out.resize(start + aligned, 0);
    write_struct(
        &mut out[start..start + size_of::<RtAttr>()],
        &RtAttr {
            rta_len: len as u16,
            rta_type: attr_type,
        },
    );
    out[start + size_of::<RtAttr>()..start + len].copy_from_slice(value);
}

fn push_attr_u32(out: &mut Vec<u8>, attr_type: u16, value: u32) {
    push_attr(out, attr_type, &value.to_ne_bytes());
}

fn push_attr_string(out: &mut Vec<u8>, attr_type: u16, value: &str) {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    push_attr(out, attr_type, &bytes);
}

pub(crate) fn address_message(request: &NlMsgHdr, port_id: u32, entry: &AddressEntry) -> Vec<u8> {
    let mut payload = payload_with(&IfAddrMsg {
        ifa_family: entry.family,
        ifa_prefixlen: entry.prefix_len,
        ifa_flags: entry.flags,
        ifa_scope: entry.scope,
        ifa_index: entry.index,
    });
    if !entry.address.is_empty() {
        push_attr(&mut payload, IFA_ADDRESS, &entry.address);
    }
    if !entry.local.is_empty() {
        push_attr(&mut payload, IFA_LOCAL, &entry.local);
    }
    if !entry.label.is_empty() {
        push_attr_string(&mut payload, IFA_LABEL, &entry.label);
    }
    netlink_message(request, port_id, RTM_NEWADDR, payload)
}

pub(crate) fn route_message(request: &NlMsgHdr, port_id: u32, entry: &RouteEntry) -> Vec<u8> {
    let mut payload = payload_with(&RtMsg {
        rtm_family: entry.family,
        rtm_dst_len: entry.dst_len,
        rtm_src_len: 0,
        rtm_tos: 0,
        rtm_table: entry.table,
        rtm_protocol: 0,
        rtm_scope: entry.scope,
        rtm_type: entry.route_type,
        rtm_flags: 0,
    });
    if !entry.dst.is_empty() {
        push_attr(&mut payload, RTA_DST, &entry.dst);
    }
    if !entry.gateway.is_empty() {
        push_attr(&mut payload, RTA_GATEWAY, &entry.gateway);
    }
    if let Some(oif) = entry.oif {
        push_attr_u32(&mut payload, RTA_OIF, oif);
    }
    netlink_message(request, port_id, RTM_NEWROUTE, payload)
}

pub(crate) fn link_message(request: &NlMsgHdr, port_id: u32, entry: &LinkEntry) -> Vec<u8> {
    let mut payload = payload_with(&IfInfoMsg {
        ifi_family: AF_UNSPEC as u8,
        ifi_pad: 0,
        ifi_type: entry.arphrd,
        ifi_index: entry.index as i32,
        ifi_flags: entry.flags,
        ifi_change: 0,
    });
    push_attr_string(&mut payload, IFLA_IFNAME, &entry.name);
    push_attr_u32(&mut payload, IFLA_MTU, entry.mtu);
    if !entry.hwaddr.is_empty() {
        push_attr(&mut payload, IFLA_ADDRESS, &entry.hwaddr);
    }
    netlink_message(request, port_id, RTM_NEWLINK, payload)
}

pub(crate) fn parse_ifinfo(payload: &[u8]) -> AxResult<IfInfoMsg> {
    if payload.len() < size_of::<IfInfoMsg>() {
        return Err(AxError::InvalidInput);
    }
    read_unaligned::<IfInfoMsg>(payload)
}

/// Iterate a fully copied NLA stream.  Netlink attribute alignment is part of
/// the ABI: accepting an unterminated padding fragment would otherwise make a
/// later message in the same write appear to be an attribute of this one.
pub(crate) fn for_each_rtattr<'a>(
    mut bytes: &'a [u8],
    mut visit: impl FnMut(u16, &'a [u8]) -> AxResult,
) -> AxResult {
    while !bytes.is_empty() {
        if bytes.len() < size_of::<RtAttr>() {
            return Err(AxError::InvalidInput);
        }
        let attr = read_unaligned::<RtAttr>(bytes)?;
        let length = attr.rta_len as usize;
        if length < size_of::<RtAttr>() || length > bytes.len() {
            return Err(AxError::InvalidInput);
        }
        visit(attr.rta_type & !0x8000, &bytes[size_of::<RtAttr>()..length])?;
        let aligned = align4(length);
        if aligned > bytes.len() {
            // The final netlink attribute does not require explicit padding.
            if length == bytes.len() {
                return Ok(());
            }
            return Err(AxError::InvalidInput);
        }
        bytes = &bytes[aligned..];
    }
    Ok(())
}

pub(crate) fn decode_nft_name(bytes: &[u8]) -> AxResult<String> {
    let name = bytes.strip_suffix(&[0]).ok_or(AxError::InvalidInput)?;
    if name.is_empty() || name.len() >= 256 || name.contains(&0) {
        return Err(AxError::InvalidInput);
    }
    nft_owned_name(core::str::from_utf8(name).map_err(|_| AxError::InvalidInput)?)
}

pub(crate) fn decode_link_name(bytes: &[u8]) -> AxResult<String> {
    let name = bytes.strip_suffix(&[0]).ok_or(AxError::InvalidInput)?;
    if name.is_empty() || name.len() > 15 || name.contains(&0) {
        return Err(AxError::InvalidInput);
    }
    core::str::from_utf8(name)
        .map(String::from)
        .map_err(|_| AxError::InvalidInput)
}

pub(crate) fn parse_link_attributes(bytes: &[u8]) -> AxResult<(Option<String>, Option<usize>)> {
    let mut name = None;
    let mut mtu = None;
    for_each_rtattr(bytes, |kind, value| match kind {
        IFLA_IFNAME if name.is_none() => {
            name = Some(decode_link_name(value)?);
            Ok(())
        }
        IFLA_MTU if mtu.is_none() && value.len() == size_of::<u32>() => {
            mtu = Some(u32::from_ne_bytes(value.try_into().unwrap()) as usize);
            Ok(())
        }
        IFLA_MTU => Err(AxError::InvalidInput),
        _ => Err(AxError::OperationNotSupported),
    })?;
    Ok((name, mtu))
}

fn ip_address_bytes(address: IpAddress) -> Vec<u8> {
    match address {
        IpAddress::Ipv4(address) => address.octets().to_vec(),
        IpAddress::Ipv6(address) => address.octets().to_vec(),
    }
}

pub(crate) fn decode_ip(family: u8, bytes: &[u8]) -> AxResult<IpAddress> {
    match family as u32 {
        value if value == AF_INET as u32 && bytes.len() == 4 => Ok(IpAddress::Ipv4(
            Ipv4Address::from_octets(bytes.try_into().map_err(|_| AxError::InvalidInput)?),
        )),
        value if value == AF_INET6 as u32 && bytes.len() == 16 => Ok(IpAddress::Ipv6(
            Ipv6Address::from_octets(bytes.try_into().map_err(|_| AxError::InvalidInput)?),
        )),
        _ => Err(AxError::InvalidInput),
    }
}

pub(crate) fn unspecified_ip(family: u8) -> AxResult<IpAddress> {
    match family as u32 {
        value if value == AF_INET as u32 => Ok(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)),
        value if value == AF_INET6 as u32 => Ok(IpAddress::Ipv6(Ipv6Address::UNSPECIFIED)),
        _ => Err(AxError::InvalidInput),
    }
}

pub(crate) fn same_ip_family(left: IpAddress, right: IpAddress) -> bool {
    matches!(
        (left, right),
        (IpAddress::Ipv4(_), IpAddress::Ipv4(_)) | (IpAddress::Ipv6(_), IpAddress::Ipv6(_))
    )
}

pub(crate) fn address_entries(interface: &InterfaceInfo) -> Vec<AddressEntry> {
    interface
        .addresses
        .iter()
        .map(|cidr| {
            let address = cidr.address();
            let family = match address {
                IpAddress::Ipv4(_) => AF_INET as u8,
                IpAddress::Ipv6(_) => AF_INET6 as u8,
            };
            let bytes = ip_address_bytes(address);
            AddressEntry {
                family,
                prefix_len: cidr.prefix_len(),
                flags: IFA_F_PERMANENT,
                scope: if interface.kind == InterfaceKind::Loopback {
                    RT_SCOPE_HOST
                } else {
                    RT_SCOPE_UNIVERSE
                },
                index: interface.index,
                local: bytes.clone(),
                address: bytes,
                label: interface.name.clone(),
            }
        })
        .collect()
}

pub(crate) fn link_entry(interface: InterfaceInfo) -> LinkEntry {
    let is_loopback = interface.kind == InterfaceKind::Loopback;
    let base = if is_loopback {
        IFF_LOOPBACK | IFF_RUNNING
    } else {
        IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST
    };
    let flags = base
        | if interface.administrative_up {
            IFF_UP
        } else {
            0
        };
    LinkEntry {
        index: interface.index,
        name: interface.name,
        flags,
        mtu: interface.mtu.min(u32::MAX as usize) as u32,
        hwaddr: interface
            .hardware_address
            .map(|address| address.to_vec())
            .unwrap_or_default(),
        arphrd: if is_loopback {
            ARPHRD_LOOPBACK
        } else {
            ARPHRD_ETHER
        },
    }
}

pub(crate) fn route_entry(route: &RouteInfo) -> RouteEntry {
    let destination = route.destination.address();
    let is_loopback = match destination {
        IpAddress::Ipv4(address) => address.is_loopback(),
        IpAddress::Ipv6(address) => address.is_loopback(),
    };
    RouteEntry {
        family: match destination {
            IpAddress::Ipv4(_) => AF_INET as u8,
            IpAddress::Ipv6(_) => AF_INET6 as u8,
        },
        dst_len: route.destination.prefix_len(),
        table: RT_TABLE_MAIN,
        scope: if is_loopback {
            RT_SCOPE_HOST
        } else if route.gateway.is_some() {
            RT_SCOPE_UNIVERSE
        } else {
            RT_SCOPE_LINK
        },
        route_type: RTN_UNICAST,
        oif: Some(route.interface_index),
        dst: if route.destination.prefix_len() == 0 {
            Vec::new()
        } else {
            ip_address_bytes(destination)
        },
        gateway: route.gateway.map(ip_address_bytes).unwrap_or_default(),
    }
}

pub(crate) fn read_unaligned<T: bytemuck::AnyBitPattern>(data: &[u8]) -> AxResult<T> {
    if data.len() < size_of::<T>() {
        return Err(AxError::InvalidInput);
    }
    Ok(bytemuck::pod_read_unaligned(&data[..size_of::<T>()]))
}

pub(crate) fn write_struct<T: bytemuck::NoUninit>(dst: &mut [u8], value: &T) {
    let bytes = bytemuck::bytes_of(value);
    dst[..bytes.len()].copy_from_slice(bytes);
}

pub(crate) fn align4(value: usize) -> usize {
    (value + 3) & !3
}
