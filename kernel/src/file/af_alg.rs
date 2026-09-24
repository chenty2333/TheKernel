//! AF_ALG address/control parsing only. No cryptographic backend is installed.
//! Binding and I/O fail closed; never return placeholder digests or plaintext.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec::Vec,
};
use core::{
    ffi::c_int,
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::net::{SOCK_SEQPACKET, cmsghdr, sockaddr, socklen_t};

use super::{FileLike, Kstat, PseudoInode, try_pseudo_inode_path};
use crate::{
    file::{FileHandle, get_typed_file},
    mm::{UserConstPtr, UserMemoryCapability, map_usercopy_error},
};

pub const AF_ALG: u32 = 38;
pub const SOL_ALG: u32 = 279;

pub const ALG_SET_KEY: u32 = 1;
pub const ALG_SET_IV: u32 = 2;
pub const ALG_SET_OP: u32 = 3;
pub const ALG_SET_AEAD_ASSOCLEN: u32 = 4;

pub const ALG_OP_DECRYPT: u32 = 0;
pub const ALG_OP_ENCRYPT: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
struct SockAddrAlgRaw {
    salg_family: u16,
    salg_type: [u8; 14],
    salg_feat: u32,
    salg_mask: u32,
    salg_name: [u8; 64],
}

#[derive(Clone, Debug)]
pub struct SockAddrAlg {
    pub alg_type: String,
    pub alg_name: String,
}

impl SockAddrAlg {
    pub fn read_from_user(
        capability: &UserMemoryCapability,
        addr: UserConstPtr<sockaddr>,
        addrlen: socklen_t,
    ) -> AxResult<Self> {
        if (addrlen as usize) < size_of::<SockAddrAlgRaw>() {
            return Err(AxError::InvalidInput);
        }

        let raw = capability
            .read_value(addr.address().as_usize() as *const SockAddrAlgRaw)
            .map_err(map_usercopy_error)?;
        if raw.salg_family as u32 != AF_ALG {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }

        Ok(Self {
            alg_type: parse_c_string_field(&raw.salg_type)?,
            alg_name: parse_c_string_field(&raw.salg_name)?,
        })
    }
}

pub(crate) struct AfAlgSendRequest {
    payload: Vec<u8>,
    ancillary_items: usize,
}

impl AfAlgSendRequest {
    pub(crate) fn prepare(payload: Vec<u8>, control: &[u8], has_name: bool) -> AxResult<Self> {
        if has_name {
            return Err(AxError::InvalidInput);
        }
        let ancillary_items = validate_send_params(control)?;
        Ok(Self {
            payload,
            ancillary_items,
        })
    }

    pub(crate) fn payload_len(&self) -> usize {
        self.payload.len()
    }

    pub(crate) fn ancillary_items(&self) -> usize {
        self.ancillary_items
    }

    #[cfg(test)]
    pub(crate) fn payload(&self) -> &[u8] {
        &self.payload
    }
}

pub struct AfAlgSocket {
    inode: PseudoInode,
    nonblocking: AtomicBool,
}

impl AfAlgSocket {
    pub fn new_listener() -> Self {
        Self {
            inode: PseudoInode::socket(),
            nonblocking: AtomicBool::new(false),
        }
    }

    pub fn from_fd(fd: c_int) -> AxResult<FileHandle<Self>> {
        get_typed_file(fd)
    }

    pub fn validate_socket_type(ty: u32, proto: u32) -> AxResult<()> {
        if ty != SOCK_SEQPACKET {
            return Err(AxError::from(LinuxError::ESOCKTNOSUPPORT));
        }
        if proto != 0 {
            return Err(AxError::from(LinuxError::EPROTONOSUPPORT));
        }
        Ok(())
    }

    pub fn bind(&self, _addr: SockAddrAlg) -> AxResult<()> {
        // Linux reports ENOENT when no implementation of the named algorithm
        // exists. Advertising success here would be a security contract.
        Err(LinuxError::ENOENT.into())
    }

    pub fn accept_request(&self) -> AxResult<Self> {
        Err(AxError::InvalidInput)
    }

    pub fn set_alg_key(&self, _key: &[u8]) -> AxResult<()> {
        Err(AxError::InvalidInput)
    }

    pub(crate) fn send_prepared(&self, _request: AfAlgSendRequest) -> AxResult<usize> {
        Err(AxError::InvalidInput)
    }

    pub(crate) fn read_with_nowait(&self, _dst: &mut super::IoDst) -> AxResult<usize> {
        Err(AxError::InvalidInput)
    }

    pub(crate) fn write_with_nowait(&self, _src: &mut super::IoSrc) -> AxResult<usize> {
        Err(AxError::InvalidInput)
    }
}

impl FileLike for AfAlgSocket {
    fn read(&self, dst: &mut super::IoDst) -> AxResult<usize> {
        self.read_with_nowait(dst)
    }

    fn write(&self, src: &mut super::IoSrc) -> AxResult<usize> {
        self.write_with_nowait(src)
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.inode.stat())
    }

    fn update_timestamps(
        &self,
        atime: Option<axfs_ng_vfs::Timestamp>,
        mtime: Option<axfs_ng_vfs::Timestamp>,
        ctime: axfs_ng_vfs::Timestamp,
    ) -> AxResult<()> {
        self.inode.update_timestamps(atime, mtime, ctime);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult<()> {
        self.nonblocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        try_pseudo_inode_path("socket", self.inode.inode())
    }
}

impl Pollable for AfAlgSocket {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

fn validate_send_params(control: &[u8]) -> AxResult<usize> {
    let mut ancillary_items = 0usize;
    let mut offset = 0usize;
    while control.len().saturating_sub(offset) >= size_of::<cmsghdr>() {
        let hdr: cmsghdr = tk_linux_usercopy::abi_read_unaligned(&control[offset..]);
        if hdr.cmsg_len < size_of::<cmsghdr>() {
            return Err(AxError::InvalidInput);
        }
        let data_start = offset
            .checked_add(size_of::<cmsghdr>())
            .ok_or(AxError::InvalidInput)?;
        let data_end = offset
            .checked_add(hdr.cmsg_len)
            .filter(|end| *end <= control.len())
            .ok_or(AxError::InvalidInput)?;

        let data = &control[data_start..data_end];
        if hdr.cmsg_level as u32 != SOL_ALG {
            return Err(AxError::InvalidInput);
        }

        match hdr.cmsg_type as u32 {
            ALG_SET_OP => {
                if data.len() != size_of::<u32>() {
                    return Err(AxError::InvalidInput);
                }
                let raw = u32::from_ne_bytes(data.try_into().unwrap());
                if !matches!(raw, ALG_OP_DECRYPT | ALG_OP_ENCRYPT) {
                    return Err(AxError::InvalidInput);
                }
            }
            ALG_SET_IV => {
                if data.len() < size_of::<u32>() {
                    return Err(AxError::InvalidInput);
                }
                let ivlen =
                    u32::from_ne_bytes(data[..size_of::<u32>()].try_into().unwrap()) as usize;
                if data.len() < size_of::<u32>() + ivlen {
                    return Err(AxError::InvalidInput);
                }
            }
            ALG_SET_AEAD_ASSOCLEN => {
                if data.len() != size_of::<u32>() {
                    return Err(AxError::InvalidInput);
                }
            }
            _ => return Err(AxError::InvalidInput),
        }

        ancillary_items = ancillary_items
            .checked_add(1)
            .ok_or(AxError::InvalidInput)?;
        offset = offset
            .checked_add(cmsg_align(hdr.cmsg_len).ok_or(AxError::InvalidInput)?)
            .ok_or(AxError::InvalidInput)?;
    }

    Ok(ancillary_items)
}

fn cmsg_align(len: usize) -> Option<usize> {
    let align = size_of::<usize>();
    len.checked_add(align - 1).map(|len| len & !(align - 1))
}

fn parse_c_string_field(bytes: &[u8]) -> AxResult<String> {
    let len = bytes.iter().position(|&it| it == 0).unwrap_or(bytes.len());
    let raw = core::str::from_utf8(&bytes[..len]).map_err(|_| AxError::InvalidInput)?;
    Ok(raw.to_string())
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use axhal::paging::{MappingFlags, PageSize};
    use axsync::Mutex;
    use linux_raw_sys::net::AF_INET;
    use memory_addr::{PAGE_SIZE_4K, VirtAddr};

    use super::*;

    fn mapped_capability() -> UserMemoryCapability {
        let mut address_space =
            crate::mm::AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K).unwrap();
        address_space
            .map(
                VirtAddr::from(0x1000),
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                crate::mm::Backend::new_alloc(VirtAddr::from(0x1000), PageSize::Size4K),
            )
            .unwrap();
        UserMemoryCapability::new(Arc::new(Mutex::new(address_space)))
    }

    fn raw_sockaddr(family: u16) -> SockAddrAlgRaw {
        let mut alg_type = [0_u8; 14];
        alg_type[..4].copy_from_slice(b"hash");
        let mut alg_name = [0_u8; 64];
        alg_name[..4].copy_from_slice(b"sha1");
        SockAddrAlgRaw {
            salg_family: family,
            salg_type: alg_type,
            salg_feat: 0,
            salg_mask: 0,
            salg_name: alg_name,
        }
    }

    #[test]
    fn unavailable_crypto_fails_closed() {
        let socket = AfAlgSocket::new_listener();
        for (alg_type, alg_name) in [
            ("hash", "sha256"),
            ("hash", "hmac(sha1)"),
            ("skcipher", "cbc(aes-generic)"),
            ("skcipher", "salsa20"),
            ("aead", "rfc7539(chacha20,poly1305)"),
        ] {
            assert_eq!(
                socket.bind(SockAddrAlg {
                    alg_type: alg_type.into(),
                    alg_name: alg_name.into(),
                }),
                Err(LinuxError::ENOENT.into())
            );
        }
        assert!(matches!(
            socket.accept_request(),
            Err(AxError::InvalidInput)
        ));
        assert_eq!(socket.set_alg_key(&[0; 16]), Err(AxError::InvalidInput));
        assert_eq!(
            socket.send_prepared(AfAlgSendRequest::prepare(Vec::new(), &[], false).unwrap()),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn sockaddr_alg_reads_from_the_explicit_capability() {
        let capability = mapped_capability();
        capability
            .write_abi_value(0x1000 as *mut SockAddrAlgRaw, raw_sockaddr(AF_ALG as _))
            .unwrap();

        let address = SockAddrAlg::read_from_user(
            &capability,
            UserConstPtr::from(0x1000),
            size_of::<SockAddrAlgRaw>() as socklen_t,
        )
        .unwrap();
        assert_eq!(address.alg_type, "hash");
        assert_eq!(address.alg_name, "sha1");
    }

    #[test]
    fn sockaddr_alg_keeps_length_and_family_errors() {
        let capability = mapped_capability();
        capability
            .write_abi_value(0x1000 as *mut SockAddrAlgRaw, raw_sockaddr(AF_INET as _))
            .unwrap();

        let short = SockAddrAlg::read_from_user(
            &capability,
            UserConstPtr::from(0x1000),
            (size_of::<SockAddrAlgRaw>() - 1) as socklen_t,
        )
        .unwrap_err();
        assert_eq!(short, AxError::InvalidInput);

        let family = SockAddrAlg::read_from_user(
            &capability,
            UserConstPtr::from(0x1000),
            size_of::<SockAddrAlgRaw>() as socklen_t,
        )
        .unwrap_err();
        assert_eq!(family, AxError::from(LinuxError::EAFNOSUPPORT));
    }
}
