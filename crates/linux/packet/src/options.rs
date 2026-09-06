use crate::{PacketError, PacketStatistics};

/// Linux `SOL_PACKET` option names pinned by the initial contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(i32)]
pub enum PacketOption {
    /// `PACKET_ADD_MEMBERSHIP`.
    AddMembership = 1,
    /// `PACKET_DROP_MEMBERSHIP`.
    DropMembership = 2,
    /// Obsolete `PACKET_RECV_OUTPUT`.
    ReceiveOutput = 3,
    /// `PACKET_RX_RING`.
    ReceiveRing = 5,
    /// Destructive `PACKET_STATISTICS` read.
    Statistics = 6,
    /// `PACKET_COPY_THRESH`.
    CopyThreshold = 7,
    /// `PACKET_AUXDATA`.
    AuxData = 8,
    /// `PACKET_ORIGDEV`.
    OriginalDevice = 9,
    /// `PACKET_VERSION`.
    Version = 10,
    /// `PACKET_HDRLEN`.
    HeaderLength = 11,
    /// `PACKET_RESERVE`.
    Reserve = 12,
    /// `PACKET_TX_RING`.
    TransmitRing = 13,
    /// `PACKET_LOSS`.
    Loss = 14,
    /// `PACKET_VNET_HDR`.
    VirtualNetHeader = 15,
    /// `PACKET_TX_TIMESTAMP`.
    TransmitTimestamp = 16,
    /// `PACKET_TIMESTAMP`.
    Timestamp = 17,
    /// `PACKET_FANOUT`.
    Fanout = 18,
    /// `PACKET_TX_HAS_OFF`.
    TransmitHasOffset = 19,
    /// `PACKET_QDISC_BYPASS`.
    QueueDisciplineBypass = 20,
    /// `PACKET_ROLLOVER_STATS`.
    RolloverStatistics = 21,
    /// `PACKET_FANOUT_DATA`.
    FanoutData = 22,
    /// `PACKET_IGNORE_OUTGOING`.
    IgnoreOutgoing = 23,
    /// `PACKET_VNET_HDR_SZ`.
    VirtualNetHeaderSize = 24,
}

impl PacketOption {
    /// Classifies a raw option number as known or unknown.
    pub const fn from_raw(raw: i32) -> Result<Self, PacketError> {
        match raw {
            1 => Ok(Self::AddMembership),
            2 => Ok(Self::DropMembership),
            3 => Ok(Self::ReceiveOutput),
            5 => Ok(Self::ReceiveRing),
            6 => Ok(Self::Statistics),
            7 => Ok(Self::CopyThreshold),
            8 => Ok(Self::AuxData),
            9 => Ok(Self::OriginalDevice),
            10 => Ok(Self::Version),
            11 => Ok(Self::HeaderLength),
            12 => Ok(Self::Reserve),
            13 => Ok(Self::TransmitRing),
            14 => Ok(Self::Loss),
            15 => Ok(Self::VirtualNetHeader),
            16 => Ok(Self::TransmitTimestamp),
            17 => Ok(Self::Timestamp),
            18 => Ok(Self::Fanout),
            19 => Ok(Self::TransmitHasOffset),
            20 => Ok(Self::QueueDisciplineBypass),
            21 => Ok(Self::RolloverStatistics),
            22 => Ok(Self::FanoutData),
            23 => Ok(Self::IgnoreOutgoing),
            24 => Ok(Self::VirtualNetHeaderSize),
            _ => Err(PacketError::UnknownPacketOption),
        }
    }

    /// Returns the pinned Linux option number.
    pub const fn raw(self) -> i32 {
        self as i32
    }
}

/// Access direction used in an explicit unsupported-option error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketOptionOperation {
    /// `getsockopt`-style read.
    Get,
    /// `setsockopt`-style write.
    Set,
}

/// Supported first-stage packet option mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetPacketOption {
    /// Set `PACKET_IGNORE_OUTGOING`; Linux accepts only integer zero or one.
    IgnoreOutgoing(bool),
}

impl SetPacketOption {
    /// Decodes a copied integer option without performing usercopy.
    pub const fn decode(raw_option: i32, integer_value: i32) -> Result<Self, PacketError> {
        let option = match PacketOption::from_raw(raw_option) {
            Ok(option) => option,
            Err(error) => return Err(error),
        };
        match option {
            PacketOption::IgnoreOutgoing => match integer_value {
                0 => Ok(Self::IgnoreOutgoing(false)),
                1 => Ok(Self::IgnoreOutgoing(true)),
                _ => Err(PacketError::InvalidPacketOptionValue),
            },
            _ => Err(PacketError::UnsupportedPacketOption {
                option,
                operation: PacketOptionOperation::Set,
            }),
        }
    }
}

/// Supported first-stage packet option query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GetPacketOption {
    /// Read `PACKET_IGNORE_OUTGOING`.
    IgnoreOutgoing,
    /// Ask the Layer 1 endpoint for one destructive `PACKET_STATISTICS` snapshot.
    Statistics,
}

impl GetPacketOption {
    /// Strictly classifies a copied option name.
    pub const fn decode(raw_option: i32) -> Result<Self, PacketError> {
        let option = match PacketOption::from_raw(raw_option) {
            Ok(option) => option,
            Err(error) => return Err(error),
        };
        match option {
            PacketOption::IgnoreOutgoing => Ok(Self::IgnoreOutgoing),
            PacketOption::Statistics => Ok(Self::Statistics),
            _ => Err(PacketError::UnsupportedPacketOption {
                option,
                operation: PacketOptionOperation::Get,
            }),
        }
    }
}

/// Typed value returned by a supported packet option query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketOptionValue {
    /// Current `PACKET_IGNORE_OUTGOING` state.
    IgnoreOutgoing(bool),
    /// Typed mapping of an endpoint-owned destructive statistics snapshot.
    Statistics(PacketStatistics),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_and_known_unsupported_options_are_distinct() {
        assert_eq!(
            SetPacketOption::decode(PacketOption::IgnoreOutgoing.raw(), 1),
            Ok(SetPacketOption::IgnoreOutgoing(true))
        );
        assert_eq!(
            SetPacketOption::decode(PacketOption::IgnoreOutgoing.raw(), -7),
            Err(PacketError::InvalidPacketOptionValue)
        );
        assert_eq!(
            GetPacketOption::decode(PacketOption::Statistics.raw()),
            Ok(GetPacketOption::Statistics)
        );
        assert_eq!(
            GetPacketOption::decode(PacketOption::ReceiveRing.raw()),
            Err(PacketError::UnsupportedPacketOption {
                option: PacketOption::ReceiveRing,
                operation: PacketOptionOperation::Get,
            })
        );
        assert_eq!(
            PacketOption::from_raw(4),
            Err(PacketError::UnknownPacketOption)
        );
    }
}
