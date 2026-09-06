use core::num::NonZeroU16;

use crate::PacketError;

/// Host-order `ETH_P_ALL`, selecting every Ethernet protocol.
pub const ETH_P_ALL: u16 = 0x0003;

/// One exact, host-order packet protocol distinct from disabled and all.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EtherType(NonZeroU16);

impl EtherType {
    /// Builds an exact host-order protocol.
    pub const fn new(host_order: u16) -> Result<Self, PacketError> {
        if host_order == 0 || host_order == ETH_P_ALL {
            return Err(PacketError::InvalidExactProtocol);
        }
        match NonZeroU16::new(host_order) {
            Some(value) => Ok(Self(value)),
            None => Err(PacketError::InvalidExactProtocol),
        }
    }

    /// Converts one explicit network-order UAPI field into an exact value.
    pub const fn from_network_order(network_order: u16) -> Result<Self, PacketError> {
        Self::new(u16::from_be(network_order))
    }

    /// Returns the normalized host-order protocol.
    pub const fn host_order(self) -> u16 {
        self.0.get()
    }

    /// Converts this value for an explicit network-order UAPI field.
    pub const fn to_network_order(self) -> u16 {
        self.host_order().to_be()
    }
}

/// Normalized host-order packet-socket protocol selection.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ProtocolSelector {
    /// Protocol zero: the socket receives no packets.
    #[default]
    Disabled,
    /// `ETH_P_ALL`: the socket receives every protocol.
    All,
    /// One exact nonzero protocol other than `ETH_P_ALL`.
    Exact(EtherType),
}

impl ProtocolSelector {
    /// Normalizes a host-order protocol without leaving duplicate sentinels.
    pub const fn from_host_order(host_order: u16) -> Self {
        match host_order {
            0 => Self::Disabled,
            ETH_P_ALL => Self::All,
            value => match NonZeroU16::new(value) {
                Some(value) => Self::Exact(EtherType(value)),
                None => Self::Disabled,
            },
        }
    }

    /// Converts a copied `socket(2)` protocol integer from network order.
    ///
    /// Linux explicitly casts the syscall's signed integer to `__be16`; retain
    /// that low-16-bit behavior instead of rejecting values Linux accepts.
    pub const fn from_network_order_i32(network_order: i32) -> Self {
        Self::from_network_order_u16(network_order as u16)
    }

    /// Converts an explicit network-order `sockaddr_ll` field.
    pub const fn from_network_order_u16(network_order: u16) -> Self {
        Self::from_host_order(u16::from_be(network_order))
    }

    /// Returns the canonical host-order selector value.
    pub const fn host_order(self) -> u16 {
        match self {
            Self::Disabled => 0,
            Self::All => ETH_P_ALL,
            Self::Exact(protocol) => protocol.host_order(),
        }
    }

    /// Converts this selector for an explicit network-order UAPI field.
    pub const fn to_network_order_u16(self) -> u16 {
        self.host_order().to_be()
    }

    /// Tests an incoming host-order protocol against this selector.
    pub const fn accepts_host_order(self, incoming: u16) -> bool {
        match self {
            Self::Disabled => false,
            Self::All => true,
            Self::Exact(protocol) => protocol.host_order() == incoming,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinels_are_canonical_and_exact_values_are_validated() {
        assert_eq!(
            ProtocolSelector::from_host_order(0),
            ProtocolSelector::Disabled
        );
        assert_eq!(
            ProtocolSelector::from_host_order(ETH_P_ALL),
            ProtocolSelector::All
        );
        let ipv4 = EtherType::new(0x0800).unwrap();
        assert_eq!(
            ProtocolSelector::from_host_order(0x0800),
            ProtocolSelector::Exact(ipv4)
        );
        assert_eq!(EtherType::new(0), Err(PacketError::InvalidExactProtocol));
        assert_eq!(
            EtherType::new(ETH_P_ALL),
            Err(PacketError::InvalidExactProtocol)
        );
    }

    #[test]
    fn network_order_is_confined_to_named_boundaries() {
        let raw = 0x0800_u16.to_be();
        let selector = ProtocolSelector::from_network_order_u16(raw);
        assert_eq!(selector.host_order(), 0x0800);
        assert_eq!(selector.to_network_order_u16(), raw);
        assert_eq!(
            ProtocolSelector::from_network_order_i32(-1).host_order(),
            u16::from_be(u16::MAX)
        );
        assert_eq!(
            ProtocolSelector::from_network_order_i32(0x1_0000),
            ProtocolSelector::Disabled
        );
    }

    #[test]
    fn matching_uses_normalized_host_order() {
        let ipv6 = ProtocolSelector::from_host_order(0x86dd);
        assert!(ipv6.accepts_host_order(0x86dd));
        assert!(!ipv6.accepts_host_order(0x0800));
        assert!(ProtocolSelector::All.accepts_host_order(0x0800));
        assert!(!ProtocolSelector::Disabled.accepts_host_order(0x0800));
    }
}
