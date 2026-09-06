use core::num::NonZeroU64;

use crate::{
    InterfaceIndex, LinkLayerAddress, LinkLayerInfo, PacketBindRequest, PacketError, PacketType,
    ProtocolSelector, SetPacketOption, SockAddrLl,
};

/// AF_PACKET socket payload mode.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PacketSocketType {
    /// `SOCK_RAW`: link-layer headers are visible and caller-supplied on send.
    Raw,
    /// `SOCK_DGRAM`: receive starts at the network header and send is cooked.
    Datagram,
}

impl PacketSocketType {
    /// Linux `SOCK_DGRAM` after the adapter strips creation flags.
    pub const SOCK_DGRAM: i32 = 2;
    /// Linux `SOCK_RAW` after the adapter strips creation flags.
    pub const SOCK_RAW: i32 = 3;

    /// Strictly decodes the base socket type.
    pub const fn from_raw(raw: i32) -> Result<Self, PacketError> {
        match raw {
            Self::SOCK_RAW => Ok(Self::Raw),
            Self::SOCK_DGRAM => Ok(Self::Datagram),
            _ => Err(PacketError::UnsupportedSocketType),
        }
    }

    /// Returns the Linux base socket type without creation flags.
    pub const fn raw(self) -> i32 {
        match self {
            Self::Raw => Self::SOCK_RAW,
            Self::Datagram => Self::SOCK_DGRAM,
        }
    }
}

/// Nonzero, non-wrapping generation for bind publication.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BindingGeneration(NonZeroU64);

impl BindingGeneration {
    const INITIAL: Self = Self(NonZeroU64::MIN);

    /// Builds a caller-visible generation, reserving zero.
    pub const fn new(raw: u64) -> Result<Self, PacketError> {
        match NonZeroU64::new(raw) {
            Some(raw) => Ok(Self(raw)),
            None => Err(PacketError::InvalidBindingGeneration),
        }
    }

    /// Returns the generation value.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Advances without wrapping into an earlier binding identity.
    pub const fn next(self) -> Result<Self, PacketError> {
        match self.get().checked_add(1) {
            Some(raw) => Self::new(raw),
            None => Err(PacketError::BindGenerationExhausted),
        }
    }
}

/// Published protocol/interface selection for one packet socket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketBinding {
    interface: InterfaceIndex,
    protocol: ProtocolSelector,
    generation: BindingGeneration,
}

impl PacketBinding {
    /// Wildcard or exact interface selection.
    pub const fn interface(self) -> InterfaceIndex {
        self.interface
    }

    /// Effective host-order protocol selection.
    pub const fn protocol(self) -> ProtocolSelector {
        self.protocol
    }

    /// Current non-wrapping publication generation.
    pub const fn generation(self) -> BindingGeneration {
        self.generation
    }
}

/// Prepared bind/rebind transition for adapter-controlled device work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BindPlan {
    expected: PacketBinding,
    replacement: PacketBinding,
}

impl BindPlan {
    /// Exact binding snapshot that must still be live at publication.
    pub const fn expected(self) -> PacketBinding {
        self.expected
    }

    /// Binding to publish after lower tap/device preparation succeeds.
    pub const fn replacement(self) -> PacketBinding {
        self.replacement
    }

    /// Returns whether the request leaves protocol and interface unchanged.
    pub fn is_noop(self) -> bool {
        self.expected.interface == self.replacement.interface
            && self.expected.protocol == self.replacement.protocol
    }
}

/// Successful bind publication outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindPublication {
    /// The requested bind already matched the live selection.
    Unchanged,
    /// A new interface/protocol selection and generation became live.
    Rebound,
}

/// Packet direction used by `PACKET_IGNORE_OUTGOING` policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryDirection {
    /// Packet received from an ingress device path.
    Incoming,
    /// Locally generated packet observed at an egress tap.
    Outgoing,
}

/// Pure reasoned result of matching one packet against socket state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryDecision {
    /// The packet matches interface, protocol, direction, and option policy.
    Deliver,
    /// The packet arrived on another exact interface.
    InterfaceMismatch,
    /// The socket's effective protocol is disabled.
    ProtocolDisabled,
    /// An incoming packet does not match the exact protocol selector.
    ProtocolMismatch,
    /// Linux egress taps deliver outgoing packets only to `ETH_P_ALL` sockets.
    OutgoingRequiresAllProtocols,
    /// `PACKET_IGNORE_OUTGOING` suppresses an otherwise eligible outgoing packet.
    OutgoingIgnored,
}

impl DeliveryDecision {
    /// Returns whether the packet should be admitted to the lower queue.
    pub const fn should_deliver(self) -> bool {
        matches!(self, Self::Deliver)
    }
}

/// First-stage reusable AF_PACKET socket state.
#[derive(Debug, Eq, PartialEq)]
pub struct PacketSocketState {
    socket_type: PacketSocketType,
    binding: PacketBinding,
    ignore_outgoing: bool,
}

impl PacketSocketState {
    /// Creates wildcard bind state with the normalized creation protocol.
    ///
    /// A non-disabled creation protocol may require immediate lower-layer tap
    /// registration; this value object does not perform that mechanism work.
    pub const fn new(socket_type: PacketSocketType, protocol: ProtocolSelector) -> Self {
        Self {
            socket_type,
            binding: PacketBinding {
                interface: InterfaceIndex::Any,
                protocol,
                generation: BindingGeneration::INITIAL,
            },
            ignore_outgoing: false,
        }
    }

    /// RAW or cooked DGRAM payload mode.
    pub const fn socket_type(&self) -> PacketSocketType {
        self.socket_type
    }

    /// Exact current binding snapshot.
    pub const fn binding(&self) -> PacketBinding {
        self.binding
    }

    /// Prepares a bind or rebind without mutating live state.
    ///
    /// Linux treats a zero `sll_protocol` on bind as retaining the socket's
    /// current protocol. Other `sockaddr_ll` fields are irrelevant to bind;
    /// the validated interface and normalized protocol are the only consumed
    /// values. A changed plan advances generation before any adapter work.
    pub fn prepare_bind(&self, request: PacketBindRequest) -> Result<BindPlan, PacketError> {
        let protocol = match request.protocol() {
            ProtocolSelector::Disabled => self.binding.protocol,
            selected => selected,
        };
        let changed =
            request.interface() != self.binding.interface || protocol != self.binding.protocol;
        let generation = if changed {
            self.binding.generation.next()?
        } else {
            self.binding.generation
        };
        Ok(BindPlan {
            expected: self.binding,
            replacement: PacketBinding {
                interface: request.interface(),
                protocol,
                generation,
            },
        })
    }

    /// Publishes a prepared plan only if its original generation and complete
    /// binding snapshot are still current.
    ///
    /// A stale plan returns an error and leaves the live state unchanged.
    pub fn publish_bind(&mut self, plan: BindPlan) -> Result<BindPublication, PacketError> {
        if self.binding != plan.expected {
            return Err(PacketError::StaleBindPlan);
        }
        let outcome = if plan.is_noop() {
            BindPublication::Unchanged
        } else {
            BindPublication::Rebound
        };
        self.binding = plan.replacement;
        Ok(outcome)
    }

    /// Produces normalized `getsockname` state.
    ///
    /// A wildcard binding has no concrete link metadata. An exact binding
    /// requires a matching caller-owned device snapshot so this crate never
    /// reads a driver registry or silently reuses stale interface metadata.
    pub fn get_name(&self, link: Option<LinkLayerInfo>) -> Result<SockAddrLl, PacketError> {
        let (hardware_type, address) = match self.binding.interface {
            InterfaceIndex::Any => (0, LinkLayerAddress::EMPTY),
            exact => {
                let link = link.ok_or(PacketError::MissingLinkLayerInfo)?;
                if link.interface() != exact {
                    return Err(PacketError::LinkLayerInfoMismatch);
                }
                (link.hardware_type(), link.address())
            }
        };
        Ok(SockAddrLl::new(
            self.binding.interface,
            self.binding.protocol,
            hardware_type,
            PacketType::HOST,
            address,
        ))
    }

    /// Applies one strictly decoded supported packet option.
    pub fn set_option(&mut self, option: SetPacketOption) {
        match option {
            SetPacketOption::IgnoreOutgoing(enabled) => self.ignore_outgoing = enabled,
        }
    }

    /// Current `PACKET_IGNORE_OUTGOING` state.
    pub const fn ignore_outgoing(&self) -> bool {
        self.ignore_outgoing
    }

    /// Matches one endpoint packet without hiding Linux's outgoing exception.
    ///
    /// Incoming packets use ordinary disabled/all/exact protocol matching.
    /// Outgoing packets are eligible only for `ETH_P_ALL`, matching Linux's
    /// egress packet-tap registration; an exact protocol may still receive a
    /// looped-back copy later as an incoming `PACKET_HOST` packet.
    pub const fn delivery_decision(
        &self,
        direction: DeliveryDirection,
        protocol_host_order: u16,
        interface: InterfaceIndex,
    ) -> DeliveryDecision {
        if interface.is_any()
            || (!self.binding.interface.is_any() && self.binding.interface.raw() != interface.raw())
        {
            return DeliveryDecision::InterfaceMismatch;
        }
        match direction {
            DeliveryDirection::Incoming => match self.binding.protocol {
                ProtocolSelector::Disabled => DeliveryDecision::ProtocolDisabled,
                ProtocolSelector::All => DeliveryDecision::Deliver,
                ProtocolSelector::Exact(protocol)
                    if protocol.host_order() == protocol_host_order =>
                {
                    DeliveryDecision::Deliver
                }
                ProtocolSelector::Exact(_) => DeliveryDecision::ProtocolMismatch,
            },
            DeliveryDirection::Outgoing => match self.binding.protocol {
                ProtocolSelector::Disabled => DeliveryDecision::ProtocolDisabled,
                ProtocolSelector::Exact(_) => DeliveryDecision::OutgoingRequiresAllProtocols,
                ProtocolSelector::All if self.ignore_outgoing => DeliveryDecision::OutgoingIgnored,
                ProtocolSelector::All => DeliveryDecision::Deliver,
            },
        }
    }

    /// Returns whether this state is eligible for Linux's outgoing tap.
    pub const fn captures_outgoing(&self) -> bool {
        matches!(self.binding.protocol, ProtocolSelector::All) && !self.ignore_outgoing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AF_PACKET, ETH_P_ALL};

    fn bind_address(interface: i32, protocol_host: u16) -> PacketBindRequest {
        PacketBindRequest::try_from_network_order_fields(
            AF_PACKET,
            protocol_host.to_be(),
            interface,
        )
        .unwrap()
    }

    #[test]
    fn bind_zero_protocol_inherits_and_rebind_advances_generation() {
        let mut state = PacketSocketState::new(PacketSocketType::Raw, ProtocolSelector::All);
        let first = state.prepare_bind(bind_address(4, 0)).unwrap();
        assert_eq!(first.replacement().protocol(), ProtocolSelector::All);
        assert_eq!(
            first.replacement().generation().get(),
            first.expected().generation().get() + 1
        );
        assert_eq!(state.publish_bind(first), Ok(BindPublication::Rebound));

        let no_change = state.prepare_bind(bind_address(4, 0)).unwrap();
        assert!(no_change.is_noop());
        assert_eq!(
            no_change.replacement().generation(),
            state.binding().generation()
        );
        assert_eq!(
            state.publish_bind(no_change),
            Ok(BindPublication::Unchanged)
        );
    }

    #[test]
    fn stale_bind_plan_never_changes_live_state() {
        let mut state = PacketSocketState::new(PacketSocketType::Datagram, ProtocolSelector::All);
        let first = state.prepare_bind(bind_address(1, 0x0800)).unwrap();
        let stale = state.prepare_bind(bind_address(2, 0x86dd)).unwrap();
        state.publish_bind(first).unwrap();
        let live = state.binding();
        assert_eq!(state.publish_bind(stale), Err(PacketError::StaleBindPlan));
        assert_eq!(state.binding(), live);
    }

    #[test]
    fn bind_generation_never_wraps_into_aba_reuse() {
        assert_eq!(
            BindingGeneration::new(0),
            Err(PacketError::InvalidBindingGeneration)
        );
        let mut state = PacketSocketState::new(PacketSocketType::Raw, ProtocolSelector::All);
        state.binding.generation = BindingGeneration::new(u64::MAX).unwrap();
        let before = state.binding();
        assert_eq!(
            state.prepare_bind(bind_address(1, 0)),
            Err(PacketError::BindGenerationExhausted)
        );
        assert_eq!(state.binding(), before);
    }

    #[test]
    fn get_name_requires_matching_metadata_only_for_exact_bind() {
        let mut state = PacketSocketState::new(PacketSocketType::Raw, ProtocolSelector::Disabled);
        let wildcard = state.get_name(None).unwrap();
        assert!(wildcard.interface().is_any());
        assert!(wildcard.address().is_empty());

        state
            .publish_bind(state.prepare_bind(bind_address(7, ETH_P_ALL)).unwrap())
            .unwrap();
        assert_eq!(state.get_name(None), Err(PacketError::MissingLinkLayerInfo));
        let wrong = LinkLayerInfo::new(
            InterfaceIndex::exact(8).unwrap(),
            1,
            LinkLayerAddress::EMPTY,
        )
        .unwrap();
        assert_eq!(
            state.get_name(Some(wrong)),
            Err(PacketError::LinkLayerInfoMismatch)
        );

        let mac = LinkLayerAddress::new([1, 2, 3, 4, 5, 6, 0, 0], 6).unwrap();
        let link = LinkLayerInfo::new(InterfaceIndex::exact(7).unwrap(), 1, mac).unwrap();
        let name = state.get_name(Some(link)).unwrap();
        assert_eq!(name.protocol(), ProtocolSelector::All);
        assert_eq!(name.hardware_type(), 1);
        assert_eq!(name.address(), mac);
    }

    #[test]
    fn outgoing_requires_all_while_exact_loopback_host_is_incoming() {
        let interface = InterfaceIndex::exact(1).unwrap();
        let exact = PacketSocketState::new(
            PacketSocketType::Raw,
            ProtocolSelector::from_host_order(0x0800),
        );
        assert_eq!(
            exact.delivery_decision(DeliveryDirection::Incoming, 0x0800, interface),
            DeliveryDecision::Deliver
        );
        assert_eq!(
            exact.delivery_decision(DeliveryDirection::Outgoing, 0x0800, interface),
            DeliveryDecision::OutgoingRequiresAllProtocols
        );
        assert!(!exact.captures_outgoing());

        let mut all = PacketSocketState::new(PacketSocketType::Raw, ProtocolSelector::All);
        assert_eq!(
            all.delivery_decision(DeliveryDirection::Outgoing, 0x0800, interface),
            DeliveryDecision::Deliver
        );
        assert!(all.captures_outgoing());
        all.set_option(SetPacketOption::IgnoreOutgoing(true));
        assert!(all.ignore_outgoing());
        assert_eq!(
            all.delivery_decision(DeliveryDirection::Outgoing, 0x0800, interface),
            DeliveryDecision::OutgoingIgnored
        );
        assert!(!all.captures_outgoing());
    }

    #[test]
    fn delivery_reasons_keep_interface_protocol_and_direction_distinct() {
        let first = InterfaceIndex::exact(1).unwrap();
        let second = InterfaceIndex::exact(2).unwrap();
        let disabled = PacketSocketState::new(PacketSocketType::Raw, ProtocolSelector::Disabled);
        assert_eq!(
            disabled.delivery_decision(DeliveryDirection::Incoming, 0x0800, first),
            DeliveryDecision::ProtocolDisabled
        );

        let mut exact = PacketSocketState::new(
            PacketSocketType::Raw,
            ProtocolSelector::from_host_order(0x0800),
        );
        exact
            .publish_bind(exact.prepare_bind(bind_address(1, 0)).unwrap())
            .unwrap();
        assert_eq!(
            exact.delivery_decision(DeliveryDirection::Incoming, 0x86dd, first),
            DeliveryDecision::ProtocolMismatch
        );
        assert_eq!(
            exact.delivery_decision(DeliveryDirection::Incoming, 0x0800, second),
            DeliveryDecision::InterfaceMismatch
        );
        assert_eq!(
            exact.delivery_decision(DeliveryDirection::Incoming, 0x0800, InterfaceIndex::Any),
            DeliveryDecision::InterfaceMismatch
        );
    }
}
