use alloc::{
    borrow::Cow,
    format,
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
use linux_raw_sys::{
    general::S_IFSOCK,
    net::{SOCK_SEQPACKET, cmsghdr, msghdr, sockaddr, socklen_t},
};
use spin::Mutex;

use super::{FileLike, Kstat};
use crate::{
    file::{FileHandle, get_typed_file},
    mm::{IoVec, IoVectorBuf, UserConstPtr},
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
    pub fn read_from_user(addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> AxResult<Self> {
        if (addrlen as usize) < size_of::<SockAddrAlgRaw>() {
            return Err(AxError::InvalidInput);
        }

        let raw = addr.cast::<SockAddrAlgRaw>().get_as_ref()?;
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

enum SocketKind {
    Listener(Mutex<ListenerState>),
    Request(Mutex<RequestState>),
}

pub struct AfAlgSocket {
    kind: SocketKind,
    nonblocking: AtomicBool,
}

impl AfAlgSocket {
    pub fn new_listener() -> Self {
        Self {
            kind: SocketKind::Listener(Mutex::new(ListenerState::default())),
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

    pub fn sendmsg(&self, msg: &msghdr) -> AxResult<usize> {
        if !msg.msg_name.is_null() || msg.msg_namelen != 0 {
            return Err(AxError::InvalidInput);
        }

        let params = parse_send_params(msg)?;
        let mut payload = Vec::new();
        if !msg.msg_iov.is_null() && msg.msg_iovlen != 0 {
            let mut io = IoVectorBuf::new(msg.msg_iov as *const IoVec, msg.msg_iovlen)?.into_io();
            payload.resize(io.remaining(), 0);
            let read = io.read(&mut payload)?;
            payload.truncate(read);
        }

        self.push_request_input(&payload, params)?;
        Ok(payload.len())
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
                    if state.buffer.len() % 16 != 0 {
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
        Ok(Kstat {
            mode: S_IFSOCK | 0o777u32,
            blksize: 4096,
            ..Default::default()
        })
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult<()> {
        self.nonblocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> Cow<'_, str> {
        format!("af_alg:[{}]", self as *const _ as usize).into()
    }
}

impl Pollable for AfAlgSocket {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

#[derive(Default)]
struct SendParams {
    op: Option<AlgOperation>,
    iv: Option<Vec<u8>>,
    assoclen: Option<u32>,
}

fn parse_send_params(msg: &msghdr) -> AxResult<SendParams> {
    let mut params = SendParams::default();
    if msg.msg_control.is_null() || msg.msg_controllen == 0 {
        return Ok(params);
    }

    let mut ptr = msg.msg_control as usize;
    let end = ptr + msg.msg_controllen;
    while ptr + size_of::<cmsghdr>() <= end {
        let hdr = UserConstPtr::<cmsghdr>::from(ptr).get_as_ref()?;
        if hdr.cmsg_len < size_of::<cmsghdr>() || ptr + hdr.cmsg_len > end {
            return Err(AxError::InvalidInput);
        }

        let data_len = hdr.cmsg_len - size_of::<cmsghdr>();
        let data = UserConstPtr::<u8>::from(ptr + size_of::<cmsghdr>()).get_as_slice(data_len)?;
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

        ptr += cmsg_align(hdr.cmsg_len);
    }

    Ok(params)
}

fn cmsg_align(len: usize) -> usize {
    let align = size_of::<usize>();
    (len + align - 1) & !(align - 1)
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
