use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::{
    ffi::c_int,
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axio::prelude::*;
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::net::{SOCK_SEQPACKET, cmsghdr, sockaddr, socklen_t};
use spin::Mutex;

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

const HASH_ALGS: &[&str] = &[
    "md5",
    "md5-generic",
    "sha1",
    "sha1-generic",
    "sha224",
    "sha224-generic",
    "sha256",
    "sha256-generic",
    "sha3-256",
    "sha3-256-generic",
    "sha3-512",
    "sha3-512-generic",
    "sm3",
    "sm3-generic",
];

const VMAC_ALGS: &[&str] = &[
    "vmac64(aes)",
    "vmac(aes)",
    "vmac64(sm4)",
    "vmac(sm4)",
    "vmac64(sm4-generic)",
    "vmac(sm4-generic)",
];

#[repr(C)]
#[derive(Clone, Copy)]
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

        let raw = unsafe {
            capability
                .read_value_uninit(addr.address().as_usize() as *const SockAddrAlgRaw)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
        if raw.salg_family as u32 != AF_ALG {
            return Err(AxError::from(LinuxError::EAFNOSUPPORT));
        }

        Ok(Self {
            alg_type: parse_c_string_field(&raw.salg_type)?,
            alg_name: parse_c_string_field(&raw.salg_name)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AlgFamily {
    Hash,
    Skcipher,
    Aead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
enum AlgOperation {
    Decrypt,
    #[default]
    Encrypt,
}

#[derive(Clone, Debug)]
struct AlgorithmBinding {
    family: AlgFamily,
    alg_name: String,
    key: Vec<u8>,
}

#[derive(Default)]
struct ListenerState {
    binding: Option<AlgorithmBinding>,
}

struct RequestState {
    binding: AlgorithmBinding,
    op: AlgOperation,
    iv: Vec<u8>,
    assoclen: u32,
    buffer: Vec<u8>,
    output: Option<Vec<u8>>,
    output_offset: usize,
    output_finalized: bool,
}

pub(crate) struct AfAlgSendRequest {
    payload: Vec<u8>,
    params: SendParams,
    ancillary_items: usize,
    has_name: bool,
}

impl AfAlgSendRequest {
    pub(crate) fn prepare(payload: Vec<u8>, control: &[u8], has_name: bool) -> AxResult<Self> {
        let (params, ancillary_items) = parse_send_params(control)?;
        Ok(Self {
            payload,
            params,
            ancillary_items,
            has_name,
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

enum SocketKind {
    Listener(Mutex<ListenerState>),
    Request(Mutex<RequestState>),
}

pub struct AfAlgSocket {
    kind: SocketKind,
    inode: PseudoInode,
    nonblocking: AtomicBool,
}

impl AfAlgSocket {
    pub fn new_listener() -> Self {
        Self {
            kind: SocketKind::Listener(Mutex::new(ListenerState::default())),
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

    pub fn bind(&self, addr: SockAddrAlg) -> AxResult<()> {
        let binding = resolve_binding(&addr)?;
        let SocketKind::Listener(state) = &self.kind else {
            return Err(AxError::InvalidInput);
        };
        state.lock().binding = Some(binding);
        Ok(())
    }

    pub fn accept_request(&self) -> AxResult<Self> {
        let SocketKind::Listener(state) = &self.kind else {
            return Err(AxError::InvalidInput);
        };
        let binding = state
            .lock()
            .binding
            .clone()
            .ok_or_else(|| AxError::from(LinuxError::EINVAL))?;

        Ok(Self {
            kind: SocketKind::Request(Mutex::new(RequestState {
                binding,
                op: AlgOperation::Encrypt,
                iv: Vec::new(),
                assoclen: 0,
                buffer: Vec::new(),
                output: None,
                output_offset: 0,
                output_finalized: false,
            })),
            inode: PseudoInode::socket(),
            nonblocking: AtomicBool::new(self.nonblocking()),
        })
    }

    pub fn set_alg_key(&self, key: &[u8]) -> AxResult<()> {
        let SocketKind::Listener(state) = &self.kind else {
            return Err(AxError::InvalidInput);
        };
        let mut state = state.lock();
        let binding = state
            .binding
            .as_mut()
            .ok_or_else(|| AxError::from(LinuxError::EINVAL))?;
        validate_key(binding, key)?;
        binding.key.clear();
        binding.key.extend_from_slice(key);
        Ok(())
    }

    pub(crate) fn send_prepared(&self, request: AfAlgSendRequest) -> AxResult<usize> {
        if request.has_name {
            return Err(AxError::InvalidInput);
        }

        let AfAlgSendRequest {
            payload,
            params,
            ancillary_items: _,
            has_name: _,
        } = request;
        let payload_len = payload.len();
        self.push_request_input(&payload, params)?;
        Ok(payload_len)
    }

    fn push_request_input(&self, data: &[u8], params: SendParams) -> AxResult<()> {
        let SocketKind::Request(state) = &self.kind else {
            return Err(AxError::InvalidInput);
        };
        let mut state = state.lock();
        state.output = None;
        state.output_offset = 0;
        state.output_finalized = false;
        if let Some(op) = params.op {
            state.op = op;
        }
        if let Some(iv) = params.iv {
            state.iv = iv;
        }
        if let Some(assoclen) = params.assoclen {
            state.assoclen = assoclen;
        }

        if state.binding.family == AlgFamily::Hash {
            return Ok(());
        }

        if !data.is_empty() {
            state.buffer.extend_from_slice(data);
        }
        Ok(())
    }

    fn prepare_output(state: &mut RequestState) -> AxResult<()> {
        if state.output.is_some() || state.output_finalized {
            return Ok(());
        }

        let output = match state.binding.family {
            AlgFamily::Hash => vec![0; 16],
            AlgFamily::Skcipher => match state.binding.alg_name.as_str() {
                "salsa20" => Vec::new(),
                "cbc(aes-generic)" => {
                    if !state.buffer.len().is_multiple_of(16) {
                        return Err(AxError::InvalidInput);
                    }
                    state.buffer.clone()
                }
                _ => return Err(AxError::from(LinuxError::ENOENT)),
            },
            AlgFamily::Aead => state.buffer.clone(),
        };
        state.output = Some(output);
        Ok(())
    }
}

impl FileLike for AfAlgSocket {
    fn read(&self, dst: &mut super::IoDst) -> AxResult<usize> {
        let SocketKind::Request(state) = &self.kind else {
            return Err(AxError::InvalidInput);
        };
        let mut state = state.lock();
        Self::prepare_output(&mut state)?;

        let output_len = state.output.as_ref().map_or(0, Vec::len);
        if state.output_offset >= output_len {
            state.output = None;
            state.output_offset = 0;
            state.output_finalized = true;
            state.buffer.clear();
            return Ok(0);
        }

        let written = {
            let output = state.output.as_deref().unwrap_or(&[]);
            dst.write(&output[state.output_offset..])?
        };
        state.output_offset += written;
        if state.output_offset >= output_len {
            state.output = None;
            state.output_offset = 0;
            state.output_finalized = true;
            state.buffer.clear();
        }
        Ok(written)
    }

    fn write(&self, src: &mut super::IoSrc) -> AxResult<usize> {
        let mut buf = vec![0; src.remaining()];
        let read = src.read(&mut buf)?;
        buf.truncate(read);
        self.push_request_input(&buf, SendParams::default())?;
        Ok(read)
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

    fn path(&self) -> AxResult<Cow<'_, str>> {
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

#[derive(Default)]
struct SendParams {
    op: Option<AlgOperation>,
    iv: Option<Vec<u8>>,
    assoclen: Option<u32>,
}

fn parse_send_params(control: &[u8]) -> AxResult<(SendParams, usize)> {
    let mut params = SendParams::default();
    let mut ancillary_items = 0usize;
    let mut offset = 0usize;
    while control.len().saturating_sub(offset) >= size_of::<cmsghdr>() {
        let hdr = unsafe {
            control
                .as_ptr()
                .add(offset)
                .cast::<cmsghdr>()
                .read_unaligned()
        };
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
                params.op = Some(match raw {
                    ALG_OP_DECRYPT => AlgOperation::Decrypt,
                    ALG_OP_ENCRYPT => AlgOperation::Encrypt,
                    _ => return Err(AxError::InvalidInput),
                });
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
                params.iv = Some(data[size_of::<u32>()..size_of::<u32>() + ivlen].to_vec());
            }
            ALG_SET_AEAD_ASSOCLEN => {
                if data.len() != size_of::<u32>() {
                    return Err(AxError::InvalidInput);
                }
                params.assoclen = Some(u32::from_ne_bytes(data.try_into().unwrap()));
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

    Ok((params, ancillary_items))
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

fn resolve_binding(addr: &SockAddrAlg) -> AxResult<AlgorithmBinding> {
    let family = match addr.alg_type.as_str() {
        "hash" if has_hash_algorithm(&addr.alg_name) => AlgFamily::Hash,
        "skcipher" if has_skcipher_algorithm(&addr.alg_name) => AlgFamily::Skcipher,
        "aead" if has_aead_algorithm(&addr.alg_name) => AlgFamily::Aead,
        _ => return Err(AxError::from(LinuxError::ENOENT)),
    };

    Ok(AlgorithmBinding {
        family,
        alg_name: addr.alg_name.clone(),
        key: Vec::new(),
    })
}

fn has_hash_algorithm(name: &str) -> bool {
    if name.starts_with("hmac(hmac(") {
        return false;
    }

    if HASH_ALGS.contains(&name) || VMAC_ALGS.contains(&name) {
        return true;
    }

    name.strip_prefix("hmac(")
        .and_then(|inner| inner.strip_suffix(')'))
        .is_some_and(|inner| HASH_ALGS.contains(&inner))
}

fn has_skcipher_algorithm(name: &str) -> bool {
    matches!(name, "salsa20" | "cbc(aes-generic)")
}

fn has_aead_algorithm(name: &str) -> bool {
    matches!(
        name,
        "rfc7539(chacha20,poly1305)" | "authenc(hmac(sha256),cbc(aes))"
    )
}

fn validate_key(binding: &AlgorithmBinding, key: &[u8]) -> AxResult<()> {
    if binding.alg_name == "authenc(hmac(sha256),cbc(aes))" && key.len() < 12 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
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
    fn sockaddr_alg_reads_from_the_explicit_capability() {
        let capability = mapped_capability();
        unsafe {
            capability
                .write_value_unchecked(0x1000 as *mut SockAddrAlgRaw, raw_sockaddr(AF_ALG as _))
                .unwrap();
        }

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
        unsafe {
            capability
                .write_value_unchecked(0x1000 as *mut SockAddrAlgRaw, raw_sockaddr(AF_INET as _))
                .unwrap();
        }

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
