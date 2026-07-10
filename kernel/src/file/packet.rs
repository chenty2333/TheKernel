use alloc::vec::Vec;
use core::{
    ffi::c_int,
    mem::{size_of, zeroed},
    ptr,
};

use axerrno::{AxError, AxResult, LinuxError};
use axnet::{InterfaceInfo, InterfaceKind, IpAddress, NetStack};
use linux_raw_sys::{
    ioctl::{
        SIOCGIFCONF, SIOCGIFFLAGS, SIOCGIFINDEX, SIOCGIFMTU, SIOCGIFNAME, SIOCSIFFLAGS, SIOCSIFMTU,
    },
    net::{AF_INET, ifconf, ifreq, in_addr, net_device_flags, sockaddr, sockaddr_in},
};

use crate::mm::UserPtr;

fn ifreq_name_eq(ifr: &ifreq, name: &[u8]) -> bool {
    let raw_name = unsafe { ifr.ifr_ifrn.ifrn_name };
    let len = raw_name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(raw_name.len());
    len == name.len()
        && raw_name[..len]
            .iter()
            .zip(name)
            .all(|(left, right)| *left as u8 == *right)
}

fn interface_by_name<'a>(
    interfaces: &'a [InterfaceInfo],
    ifr: &ifreq,
) -> Option<&'a InterfaceInfo> {
    interfaces
        .iter()
        .find(|interface| ifreq_name_eq(ifr, interface.name.as_bytes()))
}

fn interface_by_index(interfaces: &[InterfaceInfo], index: i32) -> Option<&InterfaceInfo> {
    u32::try_from(index)
        .ok()
        .and_then(|index| interfaces.iter().find(|interface| interface.index == index))
}

fn write_ifreq_name(ifr: &mut ifreq, name: &[u8]) {
    let raw_name = unsafe { &mut ifr.ifr_ifrn.ifrn_name };
    for byte in raw_name.iter_mut() {
        *byte = 0;
    }
    let max_len = raw_name.len().saturating_sub(1);
    for (dst, src) in raw_name.iter_mut().zip(name.iter().copied()).take(max_len) {
        *dst = src as _;
    }
}

fn make_ifconf_ifreq(name: &[u8], ipv4: [u8; 4]) -> ifreq {
    let mut ifr = unsafe { zeroed::<ifreq>() };
    write_ifreq_name(&mut ifr, name);

    let addr = sockaddr_in {
        sin_family: AF_INET as _,
        sin_port: 0,
        sin_addr: in_addr {
            s_addr: u32::from_be_bytes(ipv4).to_be(),
        },
        __pad: [0; 8],
    };
    unsafe {
        ptr::write(
            (&mut ifr.ifr_ifru.ifru_addr as *mut sockaddr).cast::<sockaddr_in>(),
            addr,
        );
    }

    ifr
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

fn socket_ifconf_ioctl(stack: &NetStack, arg: usize) -> AxResult<usize> {
    let ifc = UserPtr::<ifconf>::from(arg).get_as_mut()?;
    let interfaces = stack.interfaces();
    let interfaces = ipv4_interfaces(&interfaces);
    let entry_size = size_of::<ifreq>();
    let requested_len = ifc.ifc_len.max(0) as usize;
    let buf = unsafe { ifc.ifc_ifcu.ifcu_req };

    let written_len = if buf.is_null() {
        interfaces.len() * entry_size
    } else {
        let count = (requested_len / entry_size).min(interfaces.len());
        let dst = UserPtr::<ifreq>::from(buf as usize).get_as_mut_slice(count)?;
        for (slot, (interface, ipv4)) in dst.iter_mut().zip(interfaces.iter().copied()) {
            *slot = make_ifconf_ifreq(interface.name.as_bytes(), ipv4);
        }
        count * entry_size
    };
    ifc.ifc_len = written_len as c_int;
    Ok(0)
}

pub fn socket_ifreq_ioctl(stack: &NetStack, cmd: u32, arg: usize) -> AxResult<usize> {
    if cmd == SIOCGIFCONF {
        return socket_ifconf_ioctl(stack, arg);
    }

    let ifr = UserPtr::<ifreq>::from(arg).get_as_mut()?;
    let interfaces = stack.interfaces();
    match cmd {
        SIOCGIFINDEX => {
            ifr.ifr_ifru.ifru_ivalue = interface_by_name(&interfaces, ifr)
                .ok_or_else(|| AxError::from(LinuxError::ENODEV))?
                .index as i32;
            Ok(0)
        }
        SIOCGIFNAME => {
            let index = unsafe { ifr.ifr_ifru.ifru_ivalue };
            let interface = interface_by_index(&interfaces, index)
                .ok_or_else(|| AxError::from(LinuxError::ENODEV))?;
            write_ifreq_name(ifr, interface.name.as_bytes());
            Ok(0)
        }
        SIOCGIFFLAGS => {
            let interface = interface_by_name(&interfaces, ifr)
                .ok_or_else(|| AxError::from(LinuxError::ENODEV))?;
            let mut flags = net_device_flags::IFF_UP as u32 | net_device_flags::IFF_RUNNING as u32;
            if interface.kind == InterfaceKind::Loopback {
                flags |= net_device_flags::IFF_LOOPBACK as u32;
            } else {
                flags |=
                    net_device_flags::IFF_BROADCAST as u32 | net_device_flags::IFF_MULTICAST as u32;
            }
            ifr.ifr_ifru.ifru_flags = flags as i16;
            Ok(0)
        }
        SIOCGIFMTU => {
            let interface = interface_by_name(&interfaces, ifr)
                .ok_or_else(|| AxError::from(LinuxError::ENODEV))?;
            ifr.ifr_ifru.ifru_mtu = interface.mtu.min(i32::MAX as usize) as i32;
            Ok(0)
        }
        SIOCSIFFLAGS | SIOCSIFMTU => Err(LinuxError::EOPNOTSUPP.into()),
        _ => Err(AxError::from(LinuxError::ENOTTY)),
    }
}
