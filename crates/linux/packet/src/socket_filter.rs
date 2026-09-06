//! Linux classic socket-filter ancillary extensions.

/// Linux's base for classic socket-filter ancillary loads.
pub const SKF_AD_OFF: u32 = 0xffff_f000;

macro_rules! ancillary_offsets {
    ($($name:ident = $value:expr;)+) => {
        $(
            #[doc = "Linux `SKF_AD_*` ancillary selector relative to [`SKF_AD_OFF`]."]
            pub const $name: u32 = $value;
        )+
    };
}

ancillary_offsets! {
    SKF_AD_PROTOCOL = 0;
    SKF_AD_PKTTYPE = 4;
    SKF_AD_IFINDEX = 8;
    SKF_AD_NLATTR = 12;
    SKF_AD_NLATTR_NEST = 16;
    SKF_AD_MARK = 20;
    SKF_AD_QUEUE = 24;
    SKF_AD_HATYPE = 28;
    SKF_AD_RXHASH = 32;
    SKF_AD_CPU = 36;
    SKF_AD_ALU_XOR_X = 40;
    SKF_AD_VLAN_TAG = 44;
    SKF_AD_VLAN_TAG_PRESENT = 48;
    SKF_AD_PAY_OFFSET = 52;
    SKF_AD_RANDOM = 56;
    SKF_AD_VLAN_TPID = 60;
}

/// Linux socket-filter ancillary value selected by one cBPF offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketFilterAncillary {
    /// Link-layer protocol.
    Protocol,
    /// Packet direction/type.
    PacketType,
    /// Receiving interface index.
    InterfaceIndex,
    /// Socket mark.
    Mark,
    /// Receive-queue mapping.
    Queue,
    /// VLAN tag control information.
    VlanTag,
    /// VLAN-tag presence bit.
    VlanTagPresent,
    /// VLAN protocol identifier.
    VlanTpid,
}

/// Encodes a supported Linux ancillary field as its classic-BPF load offset.
pub const fn encoded_socket_filter_ancillary(ancillary: SocketFilterAncillary) -> u32 {
    SKF_AD_OFF
        + match ancillary {
            SocketFilterAncillary::Protocol => SKF_AD_PROTOCOL,
            SocketFilterAncillary::PacketType => SKF_AD_PKTTYPE,
            SocketFilterAncillary::InterfaceIndex => SKF_AD_IFINDEX,
            SocketFilterAncillary::Mark => SKF_AD_MARK,
            SocketFilterAncillary::Queue => SKF_AD_QUEUE,
            SocketFilterAncillary::VlanTag => SKF_AD_VLAN_TAG,
            SocketFilterAncillary::VlanTagPresent => SKF_AD_VLAN_TAG_PRESENT,
            SocketFilterAncillary::VlanTpid => SKF_AD_VLAN_TPID,
        }
}

/// Pure packet values consumed by the Linux socket-filter profile.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SocketFilterSnapshot {
    /// Host-order link protocol.
    pub protocol: u32,
    /// Linux packet type.
    pub pkttype: u32,
    /// Input interface index.
    pub ifindex: u32,
    /// Packet mark.
    pub mark: u32,
    /// Receive queue mapping.
    pub queue: u32,
    /// VLAN tag control information.
    pub vlan_tag: u32,
    /// VLAN-tag presence normalized to zero or one.
    pub vlan_tag_present: u32,
    /// Host-order VLAN protocol identifier.
    pub vlan_tpid: u32,
}

impl SocketFilterSnapshot {
    /// Returns the Linux-visible value for an admitted ancillary selector.
    pub const fn value(self, ancillary: SocketFilterAncillary) -> u32 {
        match ancillary {
            SocketFilterAncillary::Protocol => self.protocol,
            SocketFilterAncillary::PacketType => self.pkttype,
            SocketFilterAncillary::InterfaceIndex => self.ifindex,
            SocketFilterAncillary::Mark => self.mark,
            SocketFilterAncillary::Queue => self.queue,
            SocketFilterAncillary::VlanTag => self.vlan_tag,
            SocketFilterAncillary::VlanTagPresent => self.vlan_tag_present,
            SocketFilterAncillary::VlanTpid => self.vlan_tpid,
        }
    }
}

/// Classifies one Linux socket-filter ancillary load offset.
pub const fn classify_socket_filter_ancillary(offset: u32) -> Option<SocketFilterAncillary> {
    match offset.wrapping_sub(SKF_AD_OFF) {
        SKF_AD_PROTOCOL => Some(SocketFilterAncillary::Protocol),
        SKF_AD_PKTTYPE => Some(SocketFilterAncillary::PacketType),
        SKF_AD_IFINDEX => Some(SocketFilterAncillary::InterfaceIndex),
        SKF_AD_MARK => Some(SocketFilterAncillary::Mark),
        SKF_AD_QUEUE => Some(SocketFilterAncillary::Queue),
        SKF_AD_VLAN_TAG => Some(SocketFilterAncillary::VlanTag),
        SKF_AD_VLAN_TAG_PRESENT => Some(SocketFilterAncillary::VlanTagPresent),
        SKF_AD_VLAN_TPID => Some(SocketFilterAncillary::VlanTpid),
        _ => None,
    }
}

/// Returns whether `offset` is reserved for a Linux ancillary load.
pub const fn is_socket_filter_ancillary_offset(offset: u32) -> bool {
    matches!(
        offset.wrapping_sub(SKF_AD_OFF),
        SKF_AD_PROTOCOL
            | SKF_AD_PKTTYPE
            | SKF_AD_IFINDEX
            | SKF_AD_NLATTR
            | SKF_AD_NLATTR_NEST
            | SKF_AD_MARK
            | SKF_AD_QUEUE
            | SKF_AD_HATYPE
            | SKF_AD_RXHASH
            | SKF_AD_CPU
            | SKF_AD_ALU_XOR_X
            | SKF_AD_VLAN_TAG
            | SKF_AD_VLAN_TAG_PRESENT
            | SKF_AD_PAY_OFFSET
            | SKF_AD_RANDOM
            | SKF_AD_VLAN_TPID
    )
}

/// Resolves an admitted Linux ancillary load offset against one packet snapshot.
///
/// Reserved ancillary selectors without a stable packet-socket value return
/// `None`; callers must reject them rather than treating the offset as packet
/// bytes.
pub const fn socket_filter_ancillary_value(
    offset: u32,
    snapshot: SocketFilterSnapshot,
) -> Option<u32> {
    match classify_socket_filter_ancillary(offset) {
        Some(ancillary) => Some(snapshot.value(ancillary)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNAPSHOT: SocketFilterSnapshot = SocketFilterSnapshot {
        protocol: 0x0800,
        pkttype: 3,
        ifindex: 7,
        mark: 0xbeef,
        queue: 5,
        vlan_tag: 100,
        vlan_tag_present: 1,
        vlan_tpid: 0x8100,
    };

    #[test]
    fn maps_supported_linux_offsets_to_snapshot_values() {
        let cases = [
            (SKF_AD_PROTOCOL, 0x0800),
            (SKF_AD_PKTTYPE, 3),
            (SKF_AD_IFINDEX, 7),
            (SKF_AD_MARK, 0xbeef),
            (SKF_AD_QUEUE, 5),
            (SKF_AD_VLAN_TAG, 100),
            (SKF_AD_VLAN_TAG_PRESENT, 1),
            (SKF_AD_VLAN_TPID, 0x8100),
        ];
        for (selector, expected) in cases {
            assert_eq!(
                socket_filter_ancillary_value(SKF_AD_OFF + selector, SNAPSHOT),
                Some(expected)
            );
        }
    }

    #[test]
    fn encoder_round_trips_every_supported_field() {
        let fields = [
            SocketFilterAncillary::Protocol,
            SocketFilterAncillary::PacketType,
            SocketFilterAncillary::InterfaceIndex,
            SocketFilterAncillary::Mark,
            SocketFilterAncillary::Queue,
            SocketFilterAncillary::VlanTag,
            SocketFilterAncillary::VlanTagPresent,
            SocketFilterAncillary::VlanTpid,
        ];
        for field in fields {
            assert_eq!(
                classify_socket_filter_ancillary(encoded_socket_filter_ancillary(field)),
                Some(field)
            );
        }
    }

    #[test]
    fn reserves_unsupported_linux_offsets_without_mapping_them() {
        let offset = SKF_AD_OFF + SKF_AD_RXHASH;
        assert!(is_socket_filter_ancillary_offset(offset));
        assert_eq!(classify_socket_filter_ancillary(offset), None);
        assert_eq!(socket_filter_ancillary_value(offset, SNAPSHOT), None);
    }
}
