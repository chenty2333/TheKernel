//! FreeBSD e1000 transmit/receive descriptor helpers.
//!
//! Translated from `sys/dev/e1000/em_txrx.c` in FreeBSD commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2016 Nicole Graziano <nicole@nextbsd.org>.
//! Copyright (c) 2017 Matthew Macy <mmacy@mattmacy.io>.

use super::registers::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmRxChecksum {
    pub ip_checked: bool,
    pub ip_valid: bool,
    pub data_valid: bool,
    pub pseudo_header: bool,
    pub data: u16,
}

/// upstream: em_txrx.c em_receive_checksum()
pub fn em_receive_checksum(status: u16, errors: u8) -> EmRxChecksum {
    if u32::from(status) & E1000_RXD_STAT_IXSM != 0
        || u32::from(errors) & (E1000_RXD_ERR_IPE | E1000_RXD_ERR_TCPE) != 0
    {
        return EmRxChecksum::default();
    }
    let ip = u32::from(status) & E1000_RXD_STAT_IPCS != 0;
    let l4 = u32::from(status) & (E1000_RXD_STAT_TCPCS | E1000_RXD_STAT_UDPCS) != 0;
    EmRxChecksum {
        ip_checked: ip,
        ip_valid: ip,
        data_valid: l4,
        pseudo_header: l4,
        data: if l4 { u16::MAX } else { 0 },
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EmRssType {
    #[default]
    None,
    TcpIpv4,
    Ipv4,
    TcpIpv6,
    Ipv6Ex,
    Ipv6,
    TcpIpv6Ex,
    UdpIpv4,
    UdpIpv6,
    UdpIpv6Ex,
}

/// upstream: em_txrx.c em_determine_rsstype()
pub fn em_determine_rsstype(packet_info: u32) -> EmRssType {
    match packet_info & 0x0f {
        0x01 => EmRssType::TcpIpv4,
        0x02 => EmRssType::Ipv4,
        0x03 => EmRssType::TcpIpv6,
        0x04 => EmRssType::Ipv6Ex,
        0x05 => EmRssType::Ipv6,
        0x06 => EmRssType::TcpIpv6Ex,
        0x07 => EmRssType::UdpIpv4,
        0x08 => EmRssType::UdpIpv6,
        0x09 => EmRssType::UdpIpv6Ex,
        _ => EmRssType::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_checksum_flags_match_status_and_error_gates() {
        assert_eq!(
            em_receive_checksum((E1000_RXD_STAT_IPCS | E1000_RXD_STAT_TCPCS) as u16, 0),
            EmRxChecksum {
                ip_checked: true,
                ip_valid: true,
                data_valid: true,
                pseudo_header: true,
                data: u16::MAX,
            }
        );
        assert_eq!(
            em_receive_checksum(E1000_RXD_STAT_IPCS as u16, E1000_RXD_ERR_IPE as u8),
            EmRxChecksum::default()
        );
        assert_eq!(
            em_receive_checksum(E1000_RXD_STAT_IXSM as u16, 0),
            EmRxChecksum::default()
        );
    }

    #[test]
    fn rss_type_nibble_maps_upstream_protocol_values() {
        assert_eq!(em_determine_rsstype(0x101), EmRssType::TcpIpv4);
        assert_eq!(em_determine_rsstype(0x08), EmRssType::UdpIpv6);
        assert_eq!(em_determine_rsstype(0x0a), EmRssType::None);
    }
}
