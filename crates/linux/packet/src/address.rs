use core::{fmt, num::NonZeroI32};

use crate::{PacketError, ProtocolSelector};

/// Linux `AF_PACKET` address-family value.
pub const AF_PACKET: u16 = 17;
/// Number of bytes in Linux `sockaddr_ll::sll_addr`.
pub const MAX_LINK_LAYER_ADDRESS_LEN: usize = 8;

/// Wildcard or exact positive Linux interface index.
///
/// The representation is private so callers cannot bypass [`Self::exact`] or
/// [`Self::from_raw`] with a negative `NonZeroI32`.
///
/// ```compile_fail
/// use core::num::NonZeroI32;
/// use thekernel_linux_packet::InterfaceIndex;
///
/// let negative = NonZeroI32::new(-1).unwrap();
/// let _ = InterfaceIndex::Exact(negative);
/// ```
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InterfaceIndex(Option<NonZeroI32>);

impl InterfaceIndex {
    /// Bind to every current interface.
    #[allow(non_upper_case_globals)]
    pub const Any: Self = Self(None);

    /// Validates a copied signed Linux interface index.
    pub const fn from_raw(raw: i32) -> Result<Self, PacketError> {
        if raw < 0 {
            return Err(PacketError::InvalidInterfaceIndex);
        }
        match NonZeroI32::new(raw) {
            Some(index) => Ok(Self(Some(index))),
            None => Ok(Self::Any),
        }
    }

    /// Builds one exact positive interface index.
    pub const fn exact(raw: u32) -> Result<Self, PacketError> {
        if raw == 0 || raw > i32::MAX as u32 {
            return Err(PacketError::InvalidInterfaceIndex);
        }
        match NonZeroI32::new(raw as i32) {
            Some(index) => Ok(Self(Some(index))),
            None => Err(PacketError::InvalidInterfaceIndex),
        }
    }

    /// Returns zero for wildcard or the positive Linux interface index.
    pub const fn raw(self) -> i32 {
        match self.0 {
            Some(index) => index.get(),
            None => 0,
        }
    }

    /// Returns whether this is the wildcard interface selection.
    pub const fn is_any(self) -> bool {
        self.0.is_none()
    }

    /// Returns the exact positive interface index, or `None` for wildcard.
    pub const fn exact_value(self) -> Option<u32> {
        match self.0 {
            Some(index) => Some(index.get() as u32),
            None => None,
        }
    }
}

impl Default for InterfaceIndex {
    fn default() -> Self {
        Self::Any
    }
}

impl fmt::Debug for InterfaceIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.exact_value() {
            Some(index) => formatter.debug_tuple("Exact").field(&index).finish(),
            None => formatter.write_str("Any"),
        }
    }
}

/// Forward-compatible Linux packet classification carried by `sockaddr_ll`.
///
/// Linux ignores this field on bind/send and may extend its output vocabulary,
/// so unknown bytes are preserved rather than rejected.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PacketType(u8);

impl PacketType {
    /// Packet addressed to the local host.
    pub const HOST: Self = Self(0);
    /// Link-layer broadcast packet.
    pub const BROADCAST: Self = Self(1);
    /// Link-layer multicast packet.
    pub const MULTICAST: Self = Self(2);
    /// Packet addressed to another host while the device is promiscuous.
    pub const OTHER_HOST: Self = Self(3);
    /// Locally generated outgoing packet.
    pub const OUTGOING: Self = Self(4);
    /// Internal looped-back multicast or broadcast frame.
    pub const LOOPBACK: Self = Self(5);
    /// Packet classified for userspace by the pinned Linux UAPI.
    pub const USER: Self = Self(6);
    /// Packet classified for kernel space by the pinned Linux UAPI.
    pub const KERNEL: Self = Self(7);

    /// Preserves one copied `sll_pkttype` byte.
    pub const fn from_raw(raw: u8) -> Self {
        Self(raw)
    }

    /// Returns the Linux `sll_pkttype` value.
    pub const fn raw(self) -> u8 {
        self.0
    }
}

/// Bind-only `sockaddr_ll` fields with Linux's ignored fields excluded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketBindRequest {
    interface: InterfaceIndex,
    protocol: ProtocolSelector,
}

impl PacketBindRequest {
    /// Builds a request from normalized host-order values.
    pub const fn new(interface: InterfaceIndex, protocol: ProtocolSelector) -> Self {
        Self {
            interface,
            protocol,
        }
    }

    /// Validates the fields Linux actually consumes from a bind address.
    ///
    /// Hardware type, packet type, address length, and address bytes are
    /// intentionally absent because Linux ignores them for packet bind.
    pub const fn try_from_network_order_fields(
        family: u16,
        protocol_network_order: u16,
        interface_index: i32,
    ) -> Result<Self, PacketError> {
        if family != AF_PACKET {
            return Err(PacketError::InvalidAddressFamily);
        }
        let interface = match InterfaceIndex::from_raw(interface_index) {
            Ok(interface) => interface,
            Err(error) => return Err(error),
        };
        Ok(Self::new(
            interface,
            ProtocolSelector::from_network_order_u16(protocol_network_order),
        ))
    }

    /// Wildcard or exact requested interface.
    pub const fn interface(self) -> InterfaceIndex {
        self.interface
    }

    /// Requested normalized protocol; disabled means retain the live value.
    pub const fn protocol(self) -> ProtocolSelector {
        self.protocol
    }
}

/// Validated address fields consumed by Linux's ordinary packet send path.
///
/// This is intentionally distinct from [`SockAddrLl`]. Receive/name output
/// uses `sll_halen` to delimit a canonical address, while Linux packet send
/// treats the complete `sll_addr` field as device input and uses `sll_halen`
/// only for the enclosing sockaddr-length check. Linux also permits a declared
/// length larger than the historical eight-byte inline field when the native
/// sockaddr has a readable extension. Keeping the declaration and inline bytes
/// separate prevents either fact from corrupting the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketSendAddress {
    interface: InterfaceIndex,
    protocol: ProtocolSelector,
    declared_address_len: u8,
    raw_address: [u8; MAX_LINK_LAYER_ADDRESS_LEN],
}

impl PacketSendAddress {
    /// Validates the bounded fields copied from an explicit `sockaddr_ll`.
    ///
    /// Address family, hardware type, and packet type are absent because the
    /// Linux packet send path ignores them. A zero interface or protocol stays
    /// zero; an explicit destination never inherits those fields from bind.
    pub const fn try_from_network_order_fields(
        protocol_network_order: u16,
        interface_index: i32,
        declared_address_len: u8,
        raw_address: [u8; MAX_LINK_LAYER_ADDRESS_LEN],
    ) -> Result<Self, PacketError> {
        let interface = match InterfaceIndex::from_raw(interface_index) {
            Ok(interface) => interface,
            Err(error) => return Err(error),
        };
        Ok(Self {
            interface,
            protocol: ProtocolSelector::from_network_order_u16(protocol_network_order),
            declared_address_len,
            raw_address,
        })
    }

    /// Explicit wildcard/positive interface copied from `sll_ifindex`.
    pub const fn interface(self) -> InterfaceIndex {
        self.interface
    }

    /// Explicit protocol copied from `sll_protocol`, including zero.
    pub const fn protocol(self) -> ProtocolSelector {
        self.protocol
    }

    /// Caller-declared `sll_halen`, retained for length-contract evidence.
    ///
    /// This can exceed [`MAX_LINK_LAYER_ADDRESS_LEN`]. The native adapter must
    /// separately prove the complete declared sockaddr range readable; a
    /// device may only request bytes represented by [`raw_address`](Self::raw_address).
    pub const fn declared_address_len(self) -> u8 {
        self.declared_address_len
    }

    /// Complete raw `sll_addr` bytes, independent of `sll_halen`.
    pub const fn raw_address(self) -> [u8; MAX_LINK_LAYER_ADDRESS_LEN] {
        self.raw_address
    }

    /// Selects exactly the address width required by the target device.
    ///
    /// Linux passes `sll_addr` to the device header builder, whose address
    /// width is a device property rather than the caller's `sll_halen` value.
    pub fn address_for_device(
        self,
        device_address_len: u8,
    ) -> Result<LinkLayerAddress, PacketError> {
        LinkLayerAddress::new(self.raw_address, device_address_len)
    }
}

/// Canonical length-delimited link-layer address.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LinkLayerAddress {
    len: u8,
    bytes: [u8; MAX_LINK_LAYER_ADDRESS_LEN],
}

impl LinkLayerAddress {
    /// Empty address used for wildcard or unavailable link metadata.
    pub const EMPTY: Self = Self {
        len: 0,
        bytes: [0; MAX_LINK_LAYER_ADDRESS_LEN],
    };

    /// Validates the length and clears unused bytes to a canonical value.
    pub fn new(mut bytes: [u8; MAX_LINK_LAYER_ADDRESS_LEN], len: u8) -> Result<Self, PacketError> {
        if usize::from(len) > MAX_LINK_LAYER_ADDRESS_LEN {
            return Err(PacketError::InvalidHardwareAddressLength);
        }
        bytes[usize::from(len)..].fill(0);
        Ok(Self { len, bytes })
    }

    /// Returns the meaningful address bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// Returns the complete canonical eight-byte storage field.
    pub const fn padded_bytes(self) -> [u8; MAX_LINK_LAYER_ADDRESS_LEN] {
        self.bytes
    }

    /// Returns the Linux address length.
    pub const fn len(self) -> u8 {
        self.len
    }

    /// Returns whether the address is empty.
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

impl Default for LinkLayerAddress {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Caller-owned immutable link metadata used to complete `getsockname`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkLayerInfo {
    interface: InterfaceIndex,
    hardware_type: u16,
    address: LinkLayerAddress,
}

impl LinkLayerInfo {
    /// Builds metadata for one exact interface.
    pub const fn new(
        interface: InterfaceIndex,
        hardware_type: u16,
        address: LinkLayerAddress,
    ) -> Result<Self, PacketError> {
        if interface.is_any() {
            return Err(PacketError::InvalidInterfaceIndex);
        }
        Ok(Self {
            interface,
            hardware_type,
            address,
        })
    }

    /// Exact interface described by this snapshot.
    pub const fn interface(self) -> InterfaceIndex {
        self.interface
    }

    /// Linux ARPHRD-style hardware type supplied by the device adapter.
    pub const fn hardware_type(self) -> u16 {
        self.hardware_type
    }

    /// Link-layer address supplied by the device adapter.
    pub const fn address(self) -> LinkLayerAddress {
        self.address
    }
}

/// Validated, normalized Linux `sockaddr_ll` value object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SockAddrLl {
    interface: InterfaceIndex,
    protocol: ProtocolSelector,
    hardware_type: u16,
    packet_type: PacketType,
    address: LinkLayerAddress,
}

impl SockAddrLl {
    /// Builds a normalized value from already validated host-order fields.
    pub const fn new(
        interface: InterfaceIndex,
        protocol: ProtocolSelector,
        hardware_type: u16,
        packet_type: PacketType,
        address: LinkLayerAddress,
    ) -> Self {
        Self {
            interface,
            protocol,
            hardware_type,
            packet_type,
            address,
        }
    }

    /// Validates copied fields and explicitly converts only `sll_protocol`
    /// from network order.
    #[allow(clippy::too_many_arguments)]
    pub fn try_from_network_order_fields(
        family: u16,
        protocol_network_order: u16,
        interface_index: i32,
        hardware_type: u16,
        packet_type: u8,
        address_len: u8,
        address: [u8; MAX_LINK_LAYER_ADDRESS_LEN],
    ) -> Result<Self, PacketError> {
        if family != AF_PACKET {
            return Err(PacketError::InvalidAddressFamily);
        }
        Ok(Self::new(
            InterfaceIndex::from_raw(interface_index)?,
            ProtocolSelector::from_network_order_u16(protocol_network_order),
            hardware_type,
            PacketType::from_raw(packet_type),
            LinkLayerAddress::new(address, address_len)?,
        ))
    }

    /// Always returns `AF_PACKET`.
    pub const fn family(self) -> u16 {
        AF_PACKET
    }

    /// Normalized wildcard or exact interface selection.
    pub const fn interface(self) -> InterfaceIndex {
        self.interface
    }

    /// Normalized host-order protocol selector.
    pub const fn protocol(self) -> ProtocolSelector {
        self.protocol
    }

    /// Explicitly converts the protocol for `sll_protocol` copyout.
    pub const fn protocol_network_order(self) -> u16 {
        self.protocol.to_network_order_u16()
    }

    /// Linux ARPHRD-style hardware type.
    pub const fn hardware_type(self) -> u16 {
        self.hardware_type
    }

    /// Linux packet classification.
    pub const fn packet_type(self) -> PacketType {
        self.packet_type
    }

    /// Canonical link-layer address.
    pub const fn address(self) -> LinkLayerAddress {
        self.address
    }
}

impl From<SockAddrLl> for PacketBindRequest {
    fn from(address: SockAddrLl) -> Self {
        Self::new(address.interface(), address.protocol())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_sockaddr_conversion_validates_every_bounded_field() {
        let address = SockAddrLl::try_from_network_order_fields(
            AF_PACKET,
            0x0800_u16.to_be(),
            7,
            1,
            PacketType::BROADCAST.raw(),
            6,
            [1, 2, 3, 4, 5, 6, 99, 99],
        )
        .unwrap();
        assert_eq!(address.interface().raw(), 7);
        assert_eq!(address.protocol().host_order(), 0x0800);
        assert_eq!(address.address().as_bytes(), &[1, 2, 3, 4, 5, 6]);
        assert_eq!(address.address().padded_bytes(), [1, 2, 3, 4, 5, 6, 0, 0]);

        assert_eq!(
            SockAddrLl::try_from_network_order_fields(2, 0, 0, 0, 0, 0, [0; 8]),
            Err(PacketError::InvalidAddressFamily)
        );
        assert_eq!(
            SockAddrLl::try_from_network_order_fields(AF_PACKET, 0, -1, 0, 0, 0, [0; 8]),
            Err(PacketError::InvalidInterfaceIndex)
        );
        let extension =
            SockAddrLl::try_from_network_order_fields(AF_PACKET, 0, 0, 0, 0xff, 0, [0; 8]).unwrap();
        assert_eq!(extension.packet_type().raw(), 0xff);
        assert_eq!(
            SockAddrLl::try_from_network_order_fields(AF_PACKET, 0, 0, 0, 0, 9, [0; 8]),
            Err(PacketError::InvalidHardwareAddressLength)
        );
    }

    #[test]
    fn send_address_preserves_raw_bytes_independent_of_declared_halen() {
        let raw = [2, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xcc, 0xdd];
        let address = PacketSendAddress::try_from_network_order_fields(0, 0, 9, raw).unwrap();
        assert!(address.interface().is_any());
        assert_eq!(address.protocol(), ProtocolSelector::Disabled);
        assert_eq!(address.declared_address_len(), 9);
        assert_eq!(address.raw_address(), raw);
        assert_eq!(address.address_for_device(6).unwrap().as_bytes(), &raw[..6]);
        assert_eq!(
            PacketSendAddress::try_from_network_order_fields(0, -1, 0, raw),
            Err(PacketError::InvalidInterfaceIndex)
        );
        let maximum = PacketSendAddress::try_from_network_order_fields(0, 1, u8::MAX, raw).unwrap();
        assert_eq!(maximum.declared_address_len(), u8::MAX);
        assert_eq!(
            maximum.address_for_device(9),
            Err(PacketError::InvalidHardwareAddressLength)
        );
    }

    #[test]
    fn exact_interface_is_positive_and_representable() {
        assert_eq!(InterfaceIndex::from_raw(0).unwrap(), InterfaceIndex::Any);
        assert_eq!(InterfaceIndex::Any.exact_value(), None);
        let first = InterfaceIndex::exact(1).unwrap();
        assert_eq!(first.raw(), 1);
        assert_eq!(first.exact_value(), Some(1));
        let maximum = InterfaceIndex::from_raw(i32::MAX).unwrap();
        assert_eq!(maximum.exact_value(), Some(i32::MAX as u32));
        assert_eq!(
            InterfaceIndex::from_raw(-1),
            Err(PacketError::InvalidInterfaceIndex)
        );
        assert_eq!(
            InterfaceIndex::exact(0),
            Err(PacketError::InvalidInterfaceIndex)
        );
        assert_eq!(
            InterfaceIndex::exact(i32::MAX as u32 + 1),
            Err(PacketError::InvalidInterfaceIndex)
        );
    }

    #[test]
    fn bind_request_contains_only_fields_linux_consumes() {
        let request =
            PacketBindRequest::try_from_network_order_fields(AF_PACKET, 0x0800_u16.to_be(), 9)
                .unwrap();
        assert_eq!(request.interface().raw(), 9);
        assert_eq!(request.protocol().host_order(), 0x0800);
    }
}
