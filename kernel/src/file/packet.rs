use alloc::vec::Vec;
use core::{
    ffi::c_int,
    mem::{MaybeUninit, offset_of, size_of},
};

use axerrno::{AxError, AxResult, LinuxError};
use axnet::{InterfaceInfo, InterfaceKind, IpAddress, NetStack};
use linux_raw_sys::{
    ioctl::{
        SIOCGIFCONF, SIOCGIFFLAGS, SIOCGIFINDEX, SIOCGIFMTU, SIOCGIFNAME, SIOCSIFFLAGS, SIOCSIFMTU,
    },
    net::{AF_INET, net_device_flags},
};

use crate::{file::IoctlContext, mm::map_usercopy_error};

// These are the x86_64 Linux UAPI wire sizes.  In particular, linux_raw_sys's
// net::sockaddr is a 128-byte storage type, while struct ifreq's active union
// is only 24 bytes on this ABI.  Never use the raw ifreq type for user memory.
const IFNAMSIZ: usize = 16;
const IFREQ_UNION_OFFSET: usize = IFNAMSIZ;
const IFREQ_SIZE: usize = 40;
const IFREQ_UNION_SIZE: usize = IFREQ_SIZE - IFREQ_UNION_OFFSET;
const IFCONF_SIZE: usize = 16;
const IFCONF_LEN_OFFSET: usize = 0;
const IFCONF_PTR_OFFSET: usize = 8;
const IFREQ_SOCKADDR_FAMILY_OFFSET: usize = IFREQ_UNION_OFFSET;
const IFREQ_SOCKADDR_DATA_OFFSET: usize = IFREQ_UNION_OFFSET + 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct IfreqWire {
    name: [u8; IFNAMSIZ],
    union: [u8; IFREQ_UNION_SIZE],
}

impl IfreqWire {
    fn from_bytes(bytes: [u8; IFREQ_SIZE]) -> Self {
        let mut name = [0u8; IFNAMSIZ];
        let mut union = [0u8; IFREQ_UNION_SIZE];
        name.copy_from_slice(&bytes[..IFNAMSIZ]);
        union.copy_from_slice(&bytes[IFREQ_UNION_OFFSET..]);
        Self { name, union }
    }

    fn to_bytes(self) -> [u8; IFREQ_SIZE] {
        let mut bytes = [0u8; IFREQ_SIZE];
        bytes[..IFNAMSIZ].copy_from_slice(&self.name);
        bytes[IFREQ_UNION_OFFSET..].copy_from_slice(&self.union);
        bytes
    }

    fn ivalue(&self) -> i32 {
        i32::from_ne_bytes(self.union[..4].try_into().unwrap())
    }

    fn set_name(&mut self, name: &[u8]) {
        self.name = encode_ifreq_name(name);
    }
}

#[repr(C)]
struct IfconfWire {
    len: c_int,
    pointer: usize,
}

const _: () = assert!(size_of::<usize>() == 8);
const _: () = assert!(size_of::<c_int>() == 4);
const _: () = assert!(size_of::<IfreqWire>() == IFREQ_SIZE);
const _: () = assert!(offset_of!(IfreqWire, name) == 0);
const _: () = assert!(offset_of!(IfreqWire, union) == IFREQ_UNION_OFFSET);
const _: () = assert!(size_of::<IfconfWire>() == IFCONF_SIZE);
const _: () = assert!(offset_of!(IfconfWire, len) == IFCONF_LEN_OFFSET);
const _: () = assert!(offset_of!(IfconfWire, pointer) == IFCONF_PTR_OFFSET);

fn read_user_bytes<const N: usize>(context: &IoctlContext, address: usize) -> AxResult<[u8; N]> {
    let mut bytes = [MaybeUninit::<u8>::uninit(); N];
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

#[derive(Clone, Copy)]
enum IfreqOutput {
    Ivalue(i32),
    Flags(i16),
    Mtu(i32),
    Name { ifindex: i32 },
}

fn read_ifreq(context: &IoctlContext, address: usize) -> AxResult<IfreqWire> {
    read_user_bytes::<IFREQ_SIZE>(context, address).map(IfreqWire::from_bytes)
}

fn encode_ifreq(ifr: IfreqWire, output: IfreqOutput) -> [u8; IFREQ_SIZE] {
    // Linux imports the complete ifreq before updating the active union
    // member. Preserve all caller-owned inactive bytes while changing only
    // the field defined by this command.
    let mut bytes = ifr.to_bytes();
    match output {
        IfreqOutput::Ivalue(value) => {
            bytes[IFREQ_UNION_OFFSET..][..4].copy_from_slice(&value.to_ne_bytes());
        }
        IfreqOutput::Flags(value) => {
            bytes[IFREQ_UNION_OFFSET..][..2].copy_from_slice(&value.to_ne_bytes());
        }
        IfreqOutput::Mtu(value) => {
            bytes[IFREQ_UNION_OFFSET..][..4].copy_from_slice(&value.to_ne_bytes());
        }
        IfreqOutput::Name { ifindex } => {
            bytes[IFREQ_UNION_OFFSET..][..4].copy_from_slice(&ifindex.to_ne_bytes());
        }
    }
    bytes
}

fn write_ifreq(
    context: &IoctlContext,
    address: usize,
    ifr: IfreqWire,
    output: IfreqOutput,
) -> AxResult<()> {
    let bytes = encode_ifreq(ifr, output);
    write_user_bytes(context, address, &bytes)
}

fn ifreq_name_eq(raw_name: &[u8; IFNAMSIZ], name: &[u8]) -> bool {
    let len = raw_name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(raw_name.len());
    len == name.len() && raw_name[..len] == *name
}

fn interface_by_name<'a>(
    interfaces: &'a [InterfaceInfo],
    name: &[u8; IFNAMSIZ],
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

fn encode_ifreq_name(name: &[u8]) -> [u8; IFNAMSIZ] {
    let mut raw_name = [0u8; IFNAMSIZ];
    let max_len = raw_name.len().saturating_sub(1);
    let copied = name.len().min(max_len);
    raw_name[..copied].copy_from_slice(&name[..copied]);
    raw_name
}

fn write_ifconf_ifreq(
    context: &IoctlContext,
    address: usize,
    name: &[u8],
    ipv4: [u8; 4],
) -> AxResult<()> {
    let mut bytes = [0u8; IFREQ_SIZE];
    let name_len = name.len().min(IFNAMSIZ);
    bytes[..name_len].copy_from_slice(&name[..name_len]);
    bytes[IFREQ_SOCKADDR_FAMILY_OFFSET..][..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    bytes[IFREQ_SOCKADDR_DATA_OFFSET..][..4].copy_from_slice(&ipv4);
    write_user_bytes(context, address, &bytes)
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

fn ifconf_entry_address(base: usize, index: usize) -> AxResult<usize> {
    index
        .checked_mul(IFREQ_SIZE)
        .and_then(|offset| base.checked_add(offset))
        .ok_or(AxError::InvalidInput)
}

fn socket_ifconf_ioctl(context: &IoctlContext, stack: &NetStack, arg: usize) -> AxResult<usize> {
    let bytes = read_user_bytes::<IFCONF_SIZE>(context, arg)?;
    let requested_len = i32::from_ne_bytes(bytes[IFCONF_LEN_OFFSET..][..4].try_into().unwrap());
    let buf = usize::from_ne_bytes(bytes[IFCONF_PTR_OFFSET..][..8].try_into().unwrap());
    let interfaces = stack.interfaces();
    let interfaces = ipv4_interfaces(&interfaces);
    let requested_len = requested_len.max(0) as usize;

    let written_len = if buf == 0 {
        interfaces.len() * IFREQ_SIZE
    } else {
        let count = (requested_len / IFREQ_SIZE).min(interfaces.len());
        for (index, (interface, ipv4)) in interfaces.iter().copied().take(count).enumerate() {
            let offset = ifconf_entry_address(buf, index)?;
            write_ifconf_ifreq(context, offset, interface.name.as_bytes(), ipv4)?;
        }
        count * IFREQ_SIZE
    };
    let len_address = arg
        .checked_add(IFCONF_LEN_OFFSET)
        .ok_or(AxError::InvalidInput)?;
    write_user_bytes(context, len_address, &(written_len as c_int).to_ne_bytes())?;
    Ok(0)
}

pub fn socket_ifreq_ioctl(
    context: &IoctlContext,
    stack: &NetStack,
    cmd: u32,
    arg: usize,
) -> AxResult<usize> {
    if cmd == SIOCGIFCONF {
        return socket_ifconf_ioctl(context, stack, arg);
    }

    let interfaces = stack.interfaces();
    let (ifr, output) = match cmd {
        SIOCGIFINDEX => {
            let ifr = read_ifreq(context, arg)?;
            let index = interface_by_name(&interfaces, &ifr.name)
                .ok_or_else(|| AxError::from(LinuxError::ENODEV))?
                .index as i32;
            (ifr, IfreqOutput::Ivalue(index))
        }
        SIOCGIFNAME => {
            let mut ifr = read_ifreq(context, arg)?;
            let index = ifr.ivalue();
            let interface = interface_by_index(&interfaces, index)
                .ok_or_else(|| AxError::from(LinuxError::ENODEV))?;
            ifr.set_name(interface.name.as_bytes());
            (ifr, IfreqOutput::Name { ifindex: index })
        }
        SIOCGIFFLAGS => {
            let ifr = read_ifreq(context, arg)?;
            let interface = interface_by_name(&interfaces, &ifr.name)
                .ok_or_else(|| AxError::from(LinuxError::ENODEV))?;
            let mut flags = net_device_flags::IFF_UP as u32 | net_device_flags::IFF_RUNNING as u32;
            if interface.kind == InterfaceKind::Loopback {
                flags |= net_device_flags::IFF_LOOPBACK as u32;
            } else {
                flags |=
                    net_device_flags::IFF_BROADCAST as u32 | net_device_flags::IFF_MULTICAST as u32;
            }
            (ifr, IfreqOutput::Flags(flags as i16))
        }
        SIOCGIFMTU => {
            let ifr = read_ifreq(context, arg)?;
            let interface = interface_by_name(&interfaces, &ifr.name)
                .ok_or_else(|| AxError::from(LinuxError::ENODEV))?;
            (
                ifr,
                IfreqOutput::Mtu(interface.mtu.min(i32::MAX as usize) as i32),
            )
        }
        SIOCSIFFLAGS | SIOCSIFMTU => return Err(LinuxError::EOPNOTSUPP.into()),
        _ => return Err(AxError::from(LinuxError::ENOTTY)),
    };
    write_ifreq(context, arg, ifr, output)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x86_ifreq_wire_geometry_is_linux_uapi_size() {
        assert_eq!(IFREQ_SIZE, 40);
        assert_eq!(IFREQ_UNION_OFFSET, 16);
        assert_eq!(IFCONF_SIZE, 16);
        assert_eq!(IFCONF_PTR_OFFSET, 8);
    }

    #[test]
    fn ifreq_copyout_is_bounded_and_preserves_inactive_bytes() {
        let mut input = [0xa5; IFREQ_SIZE];
        input[..3].copy_from_slice(b"lo\0");
        let encoded = encode_ifreq(IfreqWire::from_bytes(input), IfreqOutput::Ivalue(7));

        let mut guarded = [0xcc; IFREQ_SIZE + 8];
        guarded[..IFREQ_SIZE].copy_from_slice(&encoded);
        assert_eq!(&guarded[IFREQ_SIZE..], &[0xcc; 8]);
        assert_eq!(
            &guarded[IFREQ_UNION_OFFSET..IFREQ_UNION_OFFSET + 4],
            &7i32.to_ne_bytes()
        );
        assert!(
            guarded[IFREQ_UNION_OFFSET + 4..IFREQ_SIZE]
                .iter()
                .all(|byte| *byte == 0xa5)
        );
    }

    #[test]
    fn ifconf_entries_use_a_forty_byte_stride() {
        assert_eq!(ifconf_entry_address(0x1000, 0).unwrap(), 0x1000);
        assert_eq!(ifconf_entry_address(0x1000, 1).unwrap(), 0x1028);
        assert_eq!(ifconf_entry_address(0x1000, 2).unwrap(), 0x1050);
        assert!(ifconf_entry_address(usize::MAX - 8, 1).is_err());
    }
}
