use crate::{PacketOption, PacketOptionOperation};

/// Typed policy and validation failures returned by this crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PacketError {
    /// A raw socket type is not `SOCK_RAW` or `SOCK_DGRAM`.
    UnsupportedSocketType,
    /// An exact protocol attempted to use the reserved disabled or all value.
    InvalidExactProtocol,
    /// A copied address family is not `AF_PACKET`.
    InvalidAddressFamily,
    /// A copied interface index is negative or not representable by Linux.
    InvalidInterfaceIndex,
    /// A copied link-layer address length exceeds `sockaddr_ll::sll_addr`.
    InvalidHardwareAddressLength,
    /// A supported packet option value is outside its Linux UAPI domain.
    InvalidPacketOptionValue,
    /// A caller attempted to construct the reserved zero binding generation.
    InvalidBindingGeneration,
    /// A network-header offset lies beyond the complete frame.
    InvalidFrameLayout,
    /// A captured packet length exceeds the selected RAW or DGRAM view.
    InvalidCapturedLength,
    /// A receive request contains flags outside the first-stage profile.
    UnsupportedReceiveFlags,
    /// A raw `SOL_PACKET` option number is outside the pinned vocabulary.
    UnknownPacketOption,
    /// A known option is not implemented for the requested access direction.
    UnsupportedPacketOption {
        /// The recognized Linux packet option.
        option: PacketOption,
        /// Whether the rejected operation was a get or set.
        operation: PacketOptionOperation,
    },
    /// A prepared bind no longer matches the exact live generation and state.
    StaleBindPlan,
    /// Advancing bind state would wrap and permit ABA reuse.
    BindGenerationExhausted,
    /// A bound interface has no caller-provided get-name device snapshot.
    MissingLinkLayerInfo,
    /// A get-name device snapshot names a different interface.
    LinkLayerInfoMismatch,
}
