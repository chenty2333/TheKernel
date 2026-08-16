//! Native AF_PACKET UAPI import and export.
//!
//! Linux copies an input socket address into a bounded `sockaddr_storage`
//! before protocol dispatch.  Keep that full-range copy (including ignored
//! tail bytes) here, then hand only normalized values to Layer 2.  Output uses
//! the architecture-native `sockaddr_ll` layout supplied by `linux-raw-sys`.

use core::mem::MaybeUninit;

use axerrno::{AxError, AxResult, LinuxError};
use linux_raw_sys::{
    if_packet::sockaddr_ll,
    net::{AF_PACKET, sockaddr, socklen_t},
};
use thekernel_linux_packet::{PacketBindRequest, PacketError, PacketSendAddress, SockAddrLl};

use crate::{
    file::{PACKET_SOCKADDR_STORAGE_LEN, PacketSockaddrSnapshot, packet_socket::packet_error},
    mm::{UserConstPtr, UserMemoryCapability, UserPtr, map_usercopy_error},
};

pub(super) const SOCKADDR_LL_LEN: usize = core::mem::size_of::<sockaddr_ll>();
const SOCKADDR_LL_PROTOCOL_OFFSET: usize = core::mem::offset_of!(sockaddr_ll, sll_protocol);
const SOCKADDR_LL_IFINDEX_OFFSET: usize = core::mem::offset_of!(sockaddr_ll, sll_ifindex);
const SOCKADDR_LL_HATYPE_OFFSET: usize = core::mem::offset_of!(sockaddr_ll, sll_hatype);
const SOCKADDR_LL_PKTTYPE_OFFSET: usize = core::mem::offset_of!(sockaddr_ll, sll_pkttype);
const SOCKADDR_LL_HALEN_OFFSET: usize = core::mem::offset_of!(sockaddr_ll, sll_halen);
const SOCKADDR_LL_ADDR_OFFSET: usize = core::mem::offset_of!(sockaddr_ll, sll_addr);

const _: [(); 20] = [(); SOCKADDR_LL_LEN];
const _: [(); 2] = [(); SOCKADDR_LL_PROTOCOL_OFFSET];
const _: [(); 4] = [(); SOCKADDR_LL_IFINDEX_OFFSET];
const _: [(); 8] = [(); SOCKADDR_LL_HATYPE_OFFSET];
const _: [(); 10] = [(); SOCKADDR_LL_PKTTYPE_OFFSET];
const _: [(); 11] = [(); SOCKADDR_LL_HALEN_OFFSET];
const _: [(); 12] = [(); SOCKADDR_LL_ADDR_OFFSET];

pub(super) fn snapshot_address(
    capability: &UserMemoryCapability,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
) -> AxResult<PacketSockaddrSnapshot> {
    let len = validate_snapshot_len(addrlen)?;
    let mut snapshot = [0_u8; PACKET_SOCKADDR_STORAGE_LEN];
    if len != 0 {
        capability
            .read_slice(addr.address().as_usize() as *const u8, unsafe {
                // SAFETY: `MaybeUninit<u8>` and `u8` have identical
                // layouts. The provider initializes every byte in the
                // requested prefix on success.
                core::slice::from_raw_parts_mut(
                    snapshot.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                    len,
                )
            })
            .map_err(map_usercopy_error)?;
    }
    PacketSockaddrSnapshot::new(snapshot, len)
}

fn validate_snapshot_len(addrlen: socklen_t) -> AxResult<usize> {
    let len = addrlen as usize;
    if len > PACKET_SOCKADDR_STORAGE_LEN {
        return Err(AxError::InvalidInput);
    }
    Ok(len)
}

fn field<const N: usize>(bytes: &[u8], offset: usize) -> AxResult<[u8; N]> {
    bytes
        .get(offset..offset.checked_add(N).ok_or(AxError::InvalidInput)?)
        .ok_or(AxError::InvalidInput)?
        .try_into()
        .map_err(|_| AxError::InvalidInput)
}

/// Decodes packet_bind's protocol-specific fields after the security hook.
pub(super) fn decode_bind_address(
    snapshot: &PacketSockaddrSnapshot,
) -> AxResult<PacketBindRequest> {
    if snapshot.len() < SOCKADDR_LL_LEN {
        return Err(AxError::InvalidInput);
    }
    let bytes = snapshot.bytes();
    let family = u16::from_ne_bytes(field(bytes, 0)?);
    // `packet_bind()` reports EINVAL, not EAFNOSUPPORT, for a family mismatch.
    if family as u32 != AF_PACKET {
        return Err(AxError::InvalidInput);
    }
    PacketBindRequest::try_from_network_order_fields(
        AF_PACKET as u16,
        u16::from_ne_bytes(field(bytes, SOCKADDR_LL_PROTOCOL_OFFSET)?),
        i32::from_ne_bytes(field(bytes, SOCKADDR_LL_IFINDEX_OFFSET)?),
    )
    .map_err(|error| match error {
        PacketError::InvalidInterfaceIndex => LinuxError::ENODEV.into(),
        other => packet_error(other),
    })
}

/// Decodes packet_snd's destination after the security hook. Linux does not
/// consume `sll_family` here, so it is intentionally not an extra rule.
pub(super) fn decode_send_address(
    snapshot: &PacketSockaddrSnapshot,
) -> AxResult<PacketSendAddress> {
    if snapshot.len() < SOCKADDR_LL_LEN {
        return Err(AxError::InvalidInput);
    }
    let bytes = snapshot.bytes();
    let declared_halen = bytes[SOCKADDR_LL_HALEN_OFFSET];
    let required = SOCKADDR_LL_ADDR_OFFSET
        .checked_add(usize::from(declared_halen))
        .ok_or(AxError::InvalidInput)?;
    if snapshot.len() < required {
        return Err(AxError::InvalidInput);
    }
    let raw_address = field(bytes, SOCKADDR_LL_ADDR_OFFSET)?;
    PacketSendAddress::try_from_network_order_fields(
        u16::from_ne_bytes(field(bytes, SOCKADDR_LL_PROTOCOL_OFFSET)?),
        i32::from_ne_bytes(field(bytes, SOCKADDR_LL_IFINDEX_OFFSET)?),
        declared_halen,
        raw_address,
    )
    .map_err(|error| match error {
        // packet_snd() performs a device lookup; zero or negative explicit
        // indices do not inherit the binding and fail as a missing device.
        PacketError::InvalidInterfaceIndex => LinuxError::ENXIO.into(),
        other => packet_error(other),
    })
}

fn native_address_bytes(address: SockAddrLl) -> [u8; SOCKADDR_LL_LEN] {
    // The compile-time layout assertions above make every byte range explicit.
    // Building from initialized integer bytes avoids ever exposing Rust padding.
    let mut bytes = [0_u8; SOCKADDR_LL_LEN];
    bytes[..SOCKADDR_LL_PROTOCOL_OFFSET].copy_from_slice(&(AF_PACKET as u16).to_ne_bytes());
    bytes[SOCKADDR_LL_PROTOCOL_OFFSET..SOCKADDR_LL_IFINDEX_OFFSET]
        .copy_from_slice(&address.protocol_network_order().to_ne_bytes());
    bytes[SOCKADDR_LL_IFINDEX_OFFSET..SOCKADDR_LL_HATYPE_OFFSET]
        .copy_from_slice(&address.interface().raw().to_ne_bytes());
    bytes[SOCKADDR_LL_HATYPE_OFFSET..SOCKADDR_LL_PKTTYPE_OFFSET]
        .copy_from_slice(&address.hardware_type().to_ne_bytes());
    bytes[SOCKADDR_LL_PKTTYPE_OFFSET] = address.packet_type().raw();
    bytes[SOCKADDR_LL_HALEN_OFFSET] = address.address().len();
    bytes[SOCKADDR_LL_ADDR_OFFSET..].copy_from_slice(&address.address().padded_bytes());
    bytes
}

fn write_native_address(
    capability: &UserMemoryCapability,
    address: SockAddrLl,
    addr: UserPtr<sockaddr>,
    addrlen: &mut socklen_t,
    true_len: usize,
) -> AxResult<()> {
    if *addrlen > i32::MAX as socklen_t {
        return Err(AxError::InvalidInput);
    }
    let bytes = native_address_bytes(address);
    let copied = (*addrlen as usize).min(true_len);
    if copied != 0 {
        capability
            .write_bytes(addr.address().as_usize(), &bytes[..copied])
            .map_err(map_usercopy_error)?;
    }
    *addrlen = true_len as socklen_t;
    Ok(())
}

/// Exports Linux `packet_getname`: the true length ends after the live link
/// address (12 bytes unbound, 18 for the six-byte loopback address).
pub(super) fn write_socket_name(
    capability: &UserMemoryCapability,
    address: SockAddrLl,
    addr: UserPtr<sockaddr>,
    addrlen: &mut socklen_t,
) -> AxResult<()> {
    let true_len = SOCKADDR_LL_ADDR_OFFSET + usize::from(address.address().len());
    write_native_address(capability, address, addr, addrlen, true_len)
}

/// Exports packet receive metadata.  Linux zero-fills the unused address tail
/// and reports the complete 20-byte `sockaddr_ll` record for ordinary receive.
pub(super) fn write_received_address(
    capability: &UserMemoryCapability,
    address: SockAddrLl,
    addr: UserPtr<sockaddr>,
    addrlen: &mut socklen_t,
) -> AxResult<()> {
    write_native_address(capability, address, addr, addrlen, SOCKADDR_LL_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(family: u16) -> sockaddr_ll {
        sockaddr_ll {
            sll_family: family,
            sll_protocol: 0x0800_u16.to_be(),
            sll_ifindex: 1,
            sll_hatype: 772,
            sll_pkttype: 0,
            sll_halen: 6,
            sll_addr: [0; 8],
        }
    }

    fn snapshot(native: sockaddr_ll, length: usize) -> PacketSockaddrSnapshot {
        let mut storage = [0_u8; PACKET_SOCKADDR_STORAGE_LEN];
        storage[..SOCKADDR_LL_PROTOCOL_OFFSET].copy_from_slice(&native.sll_family.to_ne_bytes());
        storage[SOCKADDR_LL_PROTOCOL_OFFSET..SOCKADDR_LL_IFINDEX_OFFSET]
            .copy_from_slice(&native.sll_protocol.to_ne_bytes());
        storage[SOCKADDR_LL_IFINDEX_OFFSET..SOCKADDR_LL_HATYPE_OFFSET]
            .copy_from_slice(&native.sll_ifindex.to_ne_bytes());
        storage[SOCKADDR_LL_HATYPE_OFFSET..SOCKADDR_LL_PKTTYPE_OFFSET]
            .copy_from_slice(&native.sll_hatype.to_ne_bytes());
        storage[SOCKADDR_LL_PKTTYPE_OFFSET] = native.sll_pkttype;
        storage[SOCKADDR_LL_HALEN_OFFSET] = native.sll_halen;
        storage[SOCKADDR_LL_ADDR_OFFSET..SOCKADDR_LL_LEN].copy_from_slice(&native.sll_addr);
        PacketSockaddrSnapshot::new(storage, length).unwrap()
    }

    #[test]
    fn bind_family_mismatch_is_einval_not_eafnosupport() {
        let error = decode_bind_address(&snapshot(native(0), SOCKADDR_LL_LEN)).unwrap_err();
        assert_eq!(error, AxError::InvalidInput);
        assert!(decode_bind_address(&snapshot(native(AF_PACKET as u16), SOCKADDR_LL_LEN)).is_ok());
    }

    #[test]
    fn bounded_import_defers_short_address_validation_but_rejects_oversize() {
        assert_eq!(
            validate_snapshot_len((SOCKADDR_LL_LEN - 1) as socklen_t),
            Ok(SOCKADDR_LL_LEN - 1)
        );
        assert_eq!(
            validate_snapshot_len(SOCKADDR_LL_LEN as socklen_t),
            Ok(SOCKADDR_LL_LEN)
        );
        assert_eq!(
            validate_snapshot_len(PACKET_SOCKADDR_STORAGE_LEN as socklen_t),
            Ok(PACKET_SOCKADDR_STORAGE_LEN)
        );
        assert_eq!(
            validate_snapshot_len((PACKET_SOCKADDR_STORAGE_LEN + 1) as socklen_t),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            decode_bind_address(&snapshot(native(AF_PACKET as u16), SOCKADDR_LL_LEN - 1)),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn send_ignores_family_but_preserves_explicit_zero_fields_and_raw_address() {
        let mut address = native(0);
        address.sll_protocol = 0;
        address.sll_halen = 0;
        address.sll_addr = [1, 2, 3, 4, 5, 6, 7, 8];

        let decoded = decode_send_address(&snapshot(address, SOCKADDR_LL_LEN)).unwrap();
        assert_eq!(decoded.interface().raw(), 1);
        assert_eq!(decoded.protocol().host_order(), 0);
        assert_eq!(decoded.declared_address_len(), 0);
        assert_eq!(decoded.raw_address(), address.sll_addr);
    }

    #[test]
    fn send_accepts_extended_declared_halen_only_with_a_readable_extension() {
        let mut address = native(AF_PACKET as u16);
        address.sll_halen = 9;
        assert_eq!(
            decode_send_address(&snapshot(address, SOCKADDR_LL_LEN)),
            Err(AxError::InvalidInput)
        );

        let decoded = decode_send_address(&snapshot(address, SOCKADDR_LL_ADDR_OFFSET + 9)).unwrap();
        assert_eq!(decoded.declared_address_len(), 9);
        assert_eq!(decoded.raw_address(), address.sll_addr);
    }

    #[test]
    fn explicit_negative_send_interface_is_enxio() {
        let mut address = native(AF_PACKET as u16);
        address.sll_ifindex = -1;
        assert_eq!(
            decode_send_address(&snapshot(address, SOCKADDR_LL_LEN)),
            Err(LinuxError::ENXIO.into())
        );
    }

    #[test]
    fn invalid_bind_interface_is_enodev_while_zero_remains_wildcard() {
        let mut address = native(AF_PACKET as u16);
        address.sll_ifindex = -1;
        assert_eq!(
            decode_bind_address(&snapshot(address, SOCKADDR_LL_LEN)),
            Err(LinuxError::ENODEV.into())
        );
        address.sll_ifindex = 0;
        assert!(decode_bind_address(&snapshot(address, SOCKADDR_LL_LEN)).is_ok());
    }

    #[test]
    fn native_lengths_keep_getname_and_receive_contracts_distinct() {
        assert_eq!(SOCKADDR_LL_LEN, 20);
        assert_eq!(SOCKADDR_LL_ADDR_OFFSET, 12);
    }
}
