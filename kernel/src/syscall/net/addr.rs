//! Wrapper for [`sockaddr`]. Using trait to convert between [`SocketAddr`] and
//! [`sockaddr`] types.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::{
    mem::{MaybeUninit, size_of},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
};

use axerrno::{AxError, AxResult, LinuxError};
#[cfg(feature = "vsock")]
use axnet::vsock::VsockAddr;
use axnet::{SocketAddrEx, unix::UnixSocketAddr};
use linux_raw_sys::net::*;
use starry_vm::{vm_read_slice, vm_write_slice};

use crate::mm::{UserConstPtr, UserPtr};

/// Trait to extend [`SocketAddr`] and its variants with methods for reading
/// from and writing to user space.
pub trait SocketAddrExt: Sized {
    /// This method attempts to interpret the data pointed to by `addr` with the
    /// given `addrlen` as a valid socket address of the implementing type.
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> AxResult<Self>;

    /// This method serializes the current socket address instance into the
    /// [`sockaddr`] structure pointed to by `addr` in user space.
    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> AxResult<()>;

    /// Gets the address family of the socket address.
    #[allow(dead_code)]
    fn family(&self) -> u16;
}

const UNIX_PATH_CAPACITY: usize = 108;
const UNIX_SOCKADDR_CAPACITY: usize = size_of::<__kernel_sa_family_t>() + UNIX_PATH_CAPACITY;
const UNIX_SOCKADDR_OUTPUT_CAPACITY: usize = UNIX_SOCKADDR_CAPACITY + 1;
const _: [(); 2] = [(); size_of::<__kernel_sa_family_t>()];

pub(super) fn read_family(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> AxResult<u16> {
    if size_of::<__kernel_sa_family_t>() > addrlen as usize {
        return Err(AxError::InvalidInput);
    }
    let mut family = [0u8; size_of::<__kernel_sa_family_t>()];
    vm_read_slice(
        addr.address().as_usize() as *const u8,
        // SAFETY: `MaybeUninit<u8>` has the same layout as `u8`, and the VM
        // read initializes every byte before returning success.
        unsafe {
            core::slice::from_raw_parts_mut(
                family.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                family.len(),
            )
        },
    )?;
    Ok(__kernel_sa_family_t::from_ne_bytes(family))
}

fn try_arc_bytes(value: &[u8]) -> AxResult<Arc<Vec<u8>>> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.extend_from_slice(value);
    Arc::try_new(owned).map_err(|_| AxError::NoMemory)
}

fn try_arc_string(value: &str) -> AxResult<Arc<String>> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(value);
    Arc::try_new(owned).map_err(|_| AxError::NoMemory)
}

fn parse_unix_socket_addr(snapshot: &[u8]) -> AxResult<UnixSocketAddr> {
    if !(size_of::<__kernel_sa_family_t>()..=UNIX_SOCKADDR_CAPACITY).contains(&snapshot.len()) {
        return Err(AxError::InvalidInput);
    }
    let family = __kernel_sa_family_t::from_ne_bytes([snapshot[0], snapshot[1]]);
    if family as u32 != AF_UNIX {
        // Keep the networking syscall layer's existing family-mismatch errno;
        // unlike malformed AF_UNIX lengths, this is an unsupported family.
        return Err(AxError::from(LinuxError::EAFNOSUPPORT));
    }

    let data = &snapshot[size_of::<__kernel_sa_family_t>()..];
    if data.is_empty() {
        return Ok(UnixSocketAddr::Unnamed);
    }
    if data[0] == 0 {
        return try_arc_bytes(&data[1..]).map(UnixSocketAddr::Abstract);
    }

    let end = data
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(data.len());
    let path = str::from_utf8(&data[..end]).map_err(|_| AxError::InvalidInput)?;
    try_arc_string(path).map(UnixSocketAddr::Path)
}
unsafe fn cast_to_slice<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) }
}

fn fill_addr(addr: UserPtr<sockaddr>, addrlen: &mut socklen_t, data: &[u8]) -> AxResult<()> {
    if *addrlen > i32::MAX as socklen_t {
        return Err(AxError::InvalidInput);
    }
    let len = (*addrlen as usize).min(data.len());
    if len != 0 {
        vm_write_slice(addr.address().as_usize() as *mut u8, &data[..len])?;
    }
    *addrlen = data.len() as _;
    Ok(())
}

fn serialize_unix_socket_addr(
    addr: &UnixSocketAddr,
) -> AxResult<([u8; UNIX_SOCKADDR_OUTPUT_CAPACITY], usize)> {
    let mut buf = [0u8; UNIX_SOCKADDR_OUTPUT_CAPACITY];
    let family_len = size_of::<__kernel_sa_family_t>();
    buf[..family_len].copy_from_slice(&(AF_UNIX as __kernel_sa_family_t).to_ne_bytes());
    let len = match addr {
        UnixSocketAddr::Unnamed => family_len,
        UnixSocketAddr::Abstract(name) => {
            if name.len() >= UNIX_PATH_CAPACITY {
                return Err(AxError::InvalidInput);
            }
            let end = family_len + 1 + name.len();
            buf[family_len] = 0;
            buf[family_len + 1..end].copy_from_slice(name);
            end
        }
        UnixSocketAddr::Path(path) => {
            if path.len() > UNIX_PATH_CAPACITY {
                return Err(AxError::InvalidInput);
            }
            let end = family_len + path.len();
            buf[family_len..end].copy_from_slice(path.as_bytes());
            buf[end] = 0;
            end + 1
        }
    };
    Ok((buf, len))
}

impl SocketAddrExt for SocketAddr {
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> AxResult<Self> {
        match read_family(addr, addrlen)? as u32 {
            AF_INET => SocketAddrV4::read_from_user(addr, addrlen).map(Self::V4),
            AF_INET6 => SocketAddrV6::read_from_user(addr, addrlen).map(Self::V6),
            _ => Err(AxError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> AxResult<()> {
        match self {
            SocketAddr::V4(v4) => v4.write_to_user(addr, addrlen),
            SocketAddr::V6(v6) => v6.write_to_user(addr, addrlen),
        }
    }

    fn family(&self) -> u16 {
        match self {
            SocketAddr::V4(v4) => v4.family(),
            SocketAddr::V6(v6) => v6.family(),
        }
    }
}

impl SocketAddrExt for SocketAddrV4 {
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> AxResult<Self> {
        if addrlen != size_of::<sockaddr_in>() as socklen_t {
            return Err(AxError::InvalidInput);
        }
        let addr_in = addr.cast::<sockaddr_in>().get_as_ref()?;
        if addr_in.sin_family as u32 != AF_INET {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }

        Ok(SocketAddrV4::new(
            Ipv4Addr::from_bits(u32::from_be(addr_in.sin_addr.s_addr)),
            u16::from_be(addr_in.sin_port),
        ))
    }

    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> AxResult<()> {
        let sockin_addr = sockaddr_in {
            sin_family: AF_INET as _,
            sin_port: self.port().to_be(),
            sin_addr: in_addr {
                s_addr: u32::from_ne_bytes(self.ip().octets()),
            },
            __pad: [0_u8; 8],
        };
        fill_addr(addr, addrlen, unsafe { cast_to_slice(&sockin_addr) })
    }

    fn family(&self) -> u16 {
        AF_INET as u16
    }
}

impl SocketAddrExt for SocketAddrV6 {
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> AxResult<Self> {
        if addrlen != size_of::<sockaddr_in6>() as socklen_t {
            return Err(AxError::InvalidInput);
        }
        let addr_in6 = addr.cast::<sockaddr_in6>().get_as_ref()?;
        if addr_in6.sin6_family as u32 != AF_INET6 {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }

        Ok(SocketAddrV6::new(
            Ipv6Addr::from(unsafe { addr_in6.sin6_addr.in6_u.u6_addr8 }),
            u16::from_be(addr_in6.sin6_port),
            u32::from_be(addr_in6.sin6_flowinfo),
            addr_in6.sin6_scope_id,
        ))
    }

    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> AxResult<()> {
        let sockin_addr = sockaddr_in6 {
            sin6_family: AF_INET6 as _,
            sin6_port: self.port().to_be(),
            sin6_flowinfo: self.flowinfo().to_be(),
            sin6_addr: in6_addr {
                in6_u: linux_raw_sys::net::in6_addr__bindgen_ty_1 {
                    u6_addr8: self.ip().octets(),
                },
            },
            sin6_scope_id: self.scope_id(),
        };
        fill_addr(addr, addrlen, unsafe { cast_to_slice(&sockin_addr) })
    }

    fn family(&self) -> u16 {
        AF_INET6 as u16
    }
}

impl SocketAddrExt for UnixSocketAddr {
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> AxResult<Self> {
        let len = addrlen as usize;
        if !(size_of::<__kernel_sa_family_t>()..=UNIX_SOCKADDR_CAPACITY).contains(&len) {
            return Err(AxError::InvalidInput);
        }

        // Linux accepts at most the two-byte family plus all 108 sun_path
        // bytes. Snapshot that bounded record in one VM operation so parsing
        // never holds a shared reference into concurrently mutable userspace.
        let mut snapshot = [0u8; UNIX_SOCKADDR_CAPACITY];
        vm_read_slice(
            addr.address().as_usize() as *const u8,
            // SAFETY: `MaybeUninit<u8>` has the same layout as `u8`, and only
            // the `len` bytes initialized by the VM read are parsed below.
            unsafe {
                core::slice::from_raw_parts_mut(
                    snapshot.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                    len,
                )
            },
        )?;
        parse_unix_socket_addr(&snapshot[..len])
    }

    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> AxResult<()> {
        let (serialized, len) = serialize_unix_socket_addr(self)?;
        fill_addr(addr, addrlen, &serialized[..len])
    }

    fn family(&self) -> u16 {
        AF_UNIX as u16
    }
}

// This type should be provided by linux_raw_sys but it's missing.
// See https://github.com/sunfishcode/linux-raw-sys/issues/169
#[cfg(feature = "vsock")]
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr_vm {
    pub svm_family: __kernel_sa_family_t,
    pub svm_reserved1: u16,
    pub svm_port: u32,
    pub svm_cid: u32,
    pub svm_zero: [u8; 4],
}

#[cfg(feature = "vsock")]
impl SocketAddrExt for VsockAddr {
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> AxResult<Self> {
        if addrlen != size_of::<sockaddr_vm>() as socklen_t {
            return Err(AxError::InvalidInput);
        }

        let addr_vsock = addr.cast::<sockaddr_vm>().get_as_ref()?;
        if addr_vsock.svm_family as u32 != AF_VSOCK {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }
        Ok(VsockAddr {
            cid: addr_vsock.svm_cid as _,
            port: addr_vsock.svm_port,
        })
    }

    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> AxResult<()> {
        let sockvm_addr = sockaddr_vm {
            svm_family: AF_VSOCK as _,
            svm_reserved1: 0,
            svm_port: self.port,
            svm_cid: self.cid as _,
            svm_zero: [0_u8; 4],
        };
        fill_addr(addr, addrlen, unsafe { cast_to_slice(&sockvm_addr) })
    }

    fn family(&self) -> u16 {
        AF_VSOCK as u16
    }
}

impl SocketAddrExt for SocketAddrEx {
    fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> AxResult<Self> {
        match read_family(addr, addrlen)? as u32 {
            AF_INET | AF_INET6 => SocketAddr::read_from_user(addr, addrlen).map(Self::Ip),
            AF_UNIX => UnixSocketAddr::read_from_user(addr, addrlen).map(Self::Unix),
            #[cfg(feature = "vsock")]
            AF_VSOCK => VsockAddr::read_from_user(addr, addrlen).map(Self::Vsock),
            _ => Err(AxError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    fn write_to_user(&self, addr: UserPtr<sockaddr>, addrlen: &mut socklen_t) -> AxResult<()> {
        match self {
            SocketAddrEx::Ip(ip_addr) => ip_addr.write_to_user(addr, addrlen),
            SocketAddrEx::Unix(unix_addr) => unix_addr.write_to_user(addr, addrlen),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(vsock_addr) => vsock_addr.write_to_user(addr, addrlen),
        }
    }

    fn family(&self) -> u16 {
        AF_INET as u16
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn unix_snapshot(data: &[u8]) -> Vec<u8> {
        let mut snapshot = Vec::with_capacity(size_of::<__kernel_sa_family_t>() + data.len());
        snapshot.extend_from_slice(&(AF_UNIX as __kernel_sa_family_t).to_ne_bytes());
        snapshot.extend_from_slice(data);
        snapshot
    }

    fn assert_invalid_len(len: usize) {
        let snapshot = vec![0u8; len];
        assert_eq!(
            parse_unix_socket_addr(&snapshot).unwrap_err(),
            AxError::InvalidInput
        );
    }

    #[test]
    fn unix_sockaddr_rejects_lengths_below_family() {
        assert_invalid_len(0);
        assert_invalid_len(1);
    }

    #[test]
    fn unix_sockaddr_family_only_is_unnamed() {
        assert!(matches!(
            parse_unix_socket_addr(&unix_snapshot(&[])).unwrap(),
            UnixSocketAddr::Unnamed
        ));
    }

    #[test]
    fn unix_sockaddr_accepts_full_abi_capacity_and_rejects_one_more_byte() {
        let full = unix_snapshot(&[b'p'; UNIX_PATH_CAPACITY]);
        let UnixSocketAddr::Path(path) = parse_unix_socket_addr(&full).unwrap() else {
            panic!("full pathname sockaddr parsed as a different address kind");
        };
        assert_eq!(path.len(), UNIX_PATH_CAPACITY);
        assert!(path.as_bytes().iter().all(|byte| *byte == b'p'));

        assert_invalid_len(UNIX_SOCKADDR_CAPACITY + 1);
    }

    #[test]
    fn unix_sockaddr_accepts_maximum_abstract_name() {
        let mut data = [b'a'; UNIX_PATH_CAPACITY];
        data[0] = 0;
        let UnixSocketAddr::Abstract(name) = parse_unix_socket_addr(&unix_snapshot(&data)).unwrap()
        else {
            panic!("maximum abstract sockaddr parsed as a different address kind");
        };
        assert_eq!(name.len(), UNIX_PATH_CAPACITY - 1);
        assert!(name.iter().all(|byte| *byte == b'a'));
    }

    #[test]
    fn parsed_unix_sockaddr_owns_its_snapshot() {
        let mut snapshot = unix_snapshot(b"owned.sock");
        let UnixSocketAddr::Path(path) = parse_unix_socket_addr(&snapshot).unwrap() else {
            panic!("pathname sockaddr parsed as a different address kind");
        };
        snapshot[size_of::<__kernel_sa_family_t>()..].fill(b'x');
        assert_eq!(path.as_str(), "owned.sock");
    }

    #[test]
    fn unix_sockaddr_rejects_invalid_utf8_pathname() {
        assert_eq!(
            parse_unix_socket_addr(&unix_snapshot(&[0xff])).unwrap_err(),
            AxError::InvalidInput
        );
    }

    #[test]
    fn unix_sockaddr_rejects_a_different_family() {
        let family = (AF_INET as __kernel_sa_family_t).to_ne_bytes();
        assert_eq!(
            parse_unix_socket_addr(&family).unwrap_err(),
            AxError::from(LinuxError::EAFNOSUPPORT)
        );
    }

    #[test]
    fn unix_path_sockaddr_uses_two_byte_family_prefix() {
        let (encoded, len) =
            serialize_unix_socket_addr(&UnixSocketAddr::Path(Arc::new(String::from("test.sock"))))
                .unwrap();
        let encoded = &encoded[..len];
        assert_eq!(
            encoded[..2],
            (AF_UNIX as __kernel_sa_family_t).to_ne_bytes()
        );
        assert_eq!(&encoded[2..11], b"test.sock");
        assert_eq!(encoded[11], 0);
    }

    #[test]
    fn unix_abstract_sockaddr_keeps_leading_nul_after_family() {
        let (encoded, len) =
            serialize_unix_socket_addr(&UnixSocketAddr::Abstract(Arc::new(b"name".to_vec())))
                .unwrap();
        let encoded = &encoded[..len];
        assert_eq!(
            encoded[..2],
            (AF_UNIX as __kernel_sa_family_t).to_ne_bytes()
        );
        assert_eq!(encoded[2], 0);
        assert_eq!(&encoded[3..], b"name");
    }

    #[test]
    fn unix_sockaddr_serialization_is_fixed_capacity_and_bounded() {
        let full_path = UnixSocketAddr::Path(Arc::new("p".repeat(UNIX_PATH_CAPACITY)));
        let (_, len) = serialize_unix_socket_addr(&full_path).unwrap();
        assert_eq!(len, UNIX_SOCKADDR_OUTPUT_CAPACITY);

        let oversized_abstract = UnixSocketAddr::Abstract(Arc::new(vec![b'a'; UNIX_PATH_CAPACITY]));
        assert_eq!(
            serialize_unix_socket_addr(&oversized_abstract).unwrap_err(),
            AxError::InvalidInput
        );
    }
}
