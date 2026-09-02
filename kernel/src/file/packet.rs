use alloc::vec::Vec;

use axerrno::{AxError, AxResult, LinuxError};
use axnet::{InterfaceInfo, InterfaceKind, IpAddress, NetStack};
use linux_raw_sys::net::net_device_flags;
use thekernel_linux_net::{
    IFCONF_SIZE, IFREQ_SIZE, IfconfWire, IfreqOutput, IfreqRequest, IfreqWire, encode_ifconf_ipv4,
    ifconf_entry_offset, ifreq_name_eq,
};

use crate::{file::IoctlContext, mm::map_usercopy_error};

fn read_user_bytes<const N: usize>(context: &IoctlContext, address: usize) -> AxResult<[u8; N]> {
    let mut bytes = [core::mem::MaybeUninit::<u8>::uninit(); N];
    context
        .user_memory()
        .read_bytes(address, &mut bytes)
        .map_err(map_usercopy_error)?;
    Ok(core::array::from_fn(|index| {
        // SAFETY: read_bytes initialized every element before returning.
        unsafe { bytes[index].assume_init() }
    }))
}

fn write_user_bytes(context: &IoctlContext, address: usize, bytes: &[u8]) -> AxResult<()> {
    context
        .user_memory()
        .write_bytes(address, bytes)
        .map_err(map_usercopy_error)
}

fn read_ifreq(context: &IoctlContext, address: usize) -> AxResult<IfreqWire> {
    IfreqWire::decode(&read_user_bytes::<IFREQ_SIZE>(context, address)?)
        .map_err(|_| AxError::InvalidInput)
}

fn interface_by_name<'a>(
    interfaces: &'a [InterfaceInfo],
    name: &[u8; thekernel_linux_net::IFNAMSIZ],
) -> Option<&'a InterfaceInfo> {
    interfaces
        .iter()
        .find(|interface| ifreq_name_eq(name, interface.name.as_bytes()))
}

fn interface_by_index(interfaces: &[InterfaceInfo], index: i32) -> Option<&InterfaceInfo> {
    u32::try_from(index)
        .ok()
        .and_then(|index| interfaces.iter().find(|interface| interface.index == index))
}

fn ipv4_interfaces(interfaces: &[InterfaceInfo]) -> Vec<(&InterfaceInfo, [u8; 4])> {
    interfaces
        .iter()
        .filter_map(|interface| {
            interface.addresses.iter().find_map(|address| {
                let IpAddress::Ipv4(address) = address.address() else {
                    return None;
                };
                Some((interface, address.octets()))
            })
        })
        .collect()
}

fn socket_ifconf_ioctl(context: &IoctlContext, stack: &NetStack, arg: usize) -> AxResult<usize> {
    let ifconf = IfconfWire::decode(&read_user_bytes::<IFCONF_SIZE>(context, arg)?)
        .map_err(|_| AxError::InvalidInput)?;
    let snapshot = stack.interfaces();
    let interfaces = ipv4_interfaces(&snapshot);
    let written_len = if ifconf.buffer == 0 {
        interfaces
            .len()
            .checked_mul(IFREQ_SIZE)
            .ok_or(AxError::InvalidInput)?
    } else {
        let count = (ifconf.requested_len / IFREQ_SIZE).min(interfaces.len());
        for (index, (interface, ipv4)) in interfaces.iter().copied().take(count).enumerate() {
            let offset = ifconf_entry_offset(index)
                .and_then(|offset| ifconf.buffer.checked_add(offset))
                .ok_or(AxError::InvalidInput)?;
            write_user_bytes(
                context,
                offset,
                &encode_ifconf_ipv4(interface.name.as_bytes(), ipv4),
            )?;
        }
        count * IFREQ_SIZE
    };
    write_user_bytes(context, arg, &(written_len as i32).to_ne_bytes())?;
    Ok(0)
}

/// Enacts interface state queries whose Linux wire decoding lives in linux-abi.
pub fn socket_ifreq_ioctl(
    context: &IoctlContext,
    stack: &NetStack,
    cmd: u32,
    arg: usize,
) -> AxResult<usize> {
    let Some(request) = IfreqRequest::decode(cmd) else {
        return Err(LinuxError::ENOTTY.into());
    };
    if request == IfreqRequest::GetConfiguration {
        return socket_ifconf_ioctl(context, stack, arg);
    }
    if matches!(request, IfreqRequest::SetFlags | IfreqRequest::SetMtu) {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    let interfaces = stack.interfaces();
    let ifr = read_ifreq(context, arg)?;
    let (ifr, output) = match request {
        IfreqRequest::GetIndex => (
            ifr,
            IfreqOutput::Integer(
                interface_by_name(&interfaces, ifr.name())
                    .ok_or(LinuxError::ENODEV)?
                    .index as i32,
            ),
        ),
        IfreqRequest::GetName => {
            let index = ifr.ivalue();
            let interface = interface_by_index(&interfaces, index).ok_or(LinuxError::ENODEV)?;
            (
                ifr.with_name(interface.name.as_bytes()),
                IfreqOutput::Index(index),
            )
        }
        IfreqRequest::GetFlags => {
            let interface = interface_by_name(&interfaces, ifr.name()).ok_or(LinuxError::ENODEV)?;
            let mut flags = net_device_flags::IFF_UP as u32 | net_device_flags::IFF_RUNNING as u32;
            if interface.kind == InterfaceKind::Loopback {
                flags |= net_device_flags::IFF_LOOPBACK as u32;
            } else {
                flags |=
                    net_device_flags::IFF_BROADCAST as u32 | net_device_flags::IFF_MULTICAST as u32;
            }
            (ifr, IfreqOutput::Flags(flags as i16))
        }
        IfreqRequest::GetMtu => (
            ifr,
            IfreqOutput::Mtu(
                interface_by_name(&interfaces, ifr.name())
                    .ok_or(LinuxError::ENODEV)?
                    .mtu
                    .min(i32::MAX as usize) as i32,
            ),
        ),
        IfreqRequest::GetConfiguration | IfreqRequest::SetFlags | IfreqRequest::SetMtu => {
            unreachable!()
        }
    };
    write_user_bytes(context, arg, &ifr.with_output(output).bytes())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linux_abi_owns_ifreq_and_ifconf_geometry() {
        assert_eq!(IFREQ_SIZE, 40);
        assert_eq!(IFCONF_SIZE, 16);
        assert_eq!(ifconf_entry_offset(2), Some(80));
    }
}
