use alloc::borrow::Cow;
use core::{
    ffi::c_int,
    mem::{size_of, zeroed},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::{
    general::S_IFSOCK,
    ioctl::{SIOCGIFCONF, SIOCGIFFLAGS, SIOCGIFINDEX, SIOCGIFNAME, SIOCSIFFLAGS, SIOCSIFMTU},
    net::{AF_INET, ifconf, ifreq, in_addr, net_device_flags, sockaddr, sockaddr_in, socklen_t},
};
use memory_addr::PAGE_SIZE_4K;
use spin::Mutex;

use super::{FileHandle, FileLike, Kstat, get_typed_file};
use crate::mm::{UserConstPtr, UserPtr};

pub const SOL_PACKET: u32 = 263;
pub const PACKET_RX_RING: u32 = 5;
pub const PACKET_VERSION: u32 = 10;
pub const PACKET_RESERVE: u32 = 12;
pub const PACKET_VNET_HDR: u32 = 15;
pub const PACKET_FANOUT: u32 = 18;

const TPACKET_V1: i32 = 0;
const TPACKET_V2: i32 = 1;
const TPACKET_V3: i32 = 2;
const TPACKET_ALIGNMENT: u32 = 16;
const TPACKET_HDRLEN: u32 = 52;
const TPACKET2_HDRLEN: u32 = 52;
const TPACKET3_HDRLEN: u32 = 68;
const TPACKET3_BLOCK_HDRLEN: u64 = 48;
const TPACKET3_PRIV_ALIGNMENT: u64 = 8;

const IFCONF_INTERFACES: &[(&[u8], [u8; 4])] =
    &[(b"lo", [127, 0, 0, 1]), (b"eth0", [10, 0, 2, 15])];

#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrLl {
    sll_family: u16,
    sll_protocol: u16,
    sll_ifindex: i32,
    sll_hatype: u16,
    sll_pkttype: u8,
    sll_halen: u8,
    sll_addr: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TpacketReq {
    pub tp_block_size: u32,
    pub tp_block_nr: u32,
    pub tp_frame_size: u32,
    pub tp_frame_nr: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TpacketReq3 {
    pub tp_block_size: u32,
    pub tp_block_nr: u32,
    pub tp_frame_size: u32,
    pub tp_frame_nr: u32,
    pub tp_retire_blk_tov: u32,
    pub tp_sizeof_priv: u32,
    pub tp_feature_req_word: u32,
}

impl From<TpacketReq> for TpacketReq3 {
    fn from(req: TpacketReq) -> Self {
        Self {
            tp_block_size: req.tp_block_size,
            tp_block_nr: req.tp_block_nr,
            tp_frame_size: req.tp_frame_size,
            tp_frame_nr: req.tp_frame_nr,
            tp_retire_blk_tov: 0,
            tp_sizeof_priv: 0,
            tp_feature_req_word: 0,
        }
    }
}

fn tpacket_header_len(version: i32) -> u32 {
    match version {
        TPACKET_V1 => TPACKET_HDRLEN,
        TPACKET_V2 => TPACKET2_HDRLEN,
        TPACKET_V3 => TPACKET3_HDRLEN,
        _ => TPACKET_HDRLEN,
    }
}

fn align_u64(value: u64, alignment: u64) -> Option<u64> {
    debug_assert!(alignment.is_power_of_two());
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

struct PacketSocketState {
    ifindex: i32,
    version: i32,
    reserve: u32,
    vnet_hdr: bool,
    fanout: Option<u32>,
    rx_ring: Option<TpacketReq3>,
}

pub struct PacketSocket {
    protocol: u16,
    nonblocking: AtomicBool,
    state: Mutex<PacketSocketState>,
}

impl PacketSocket {
    pub fn new(protocol: u16) -> Self {
        Self {
            protocol,
            nonblocking: AtomicBool::new(false),
            state: Mutex::new(PacketSocketState {
                ifindex: 0,
                version: TPACKET_V1,
                reserve: 0,
                vnet_hdr: false,
                fanout: None,
                rx_ring: None,
            }),
        }
    }

    pub fn from_fd(fd: c_int) -> AxResult<FileHandle<Self>> {
        get_typed_file(fd)
    }

    pub fn set_packet_version(&self, version: i32) -> AxResult<()> {
        match version {
            TPACKET_V1 | TPACKET_V2 | TPACKET_V3 => {}
            _ => return Err(AxError::InvalidInput),
        }

        let mut state = self.state.lock();
        if state.rx_ring.is_some() {
            return Err(AxError::ResourceBusy);
        }
        state.version = version;
        Ok(())
    }

    pub fn packet_version(&self) -> i32 {
        self.state.lock().version
    }

    pub fn set_packet_reserve(&self, reserve: u32) -> AxResult<()> {
        if reserve > i32::MAX as u32 {
            return Err(AxError::InvalidInput);
        }

        let mut state = self.state.lock();
        if state.rx_ring.is_some() {
            return Err(AxError::ResourceBusy);
        }
        state.reserve = reserve;
        Ok(())
    }

    pub fn packet_reserve(&self) -> u32 {
        self.state.lock().reserve
    }

    pub fn set_vnet_hdr(&self, enabled: bool) {
        self.state.lock().vnet_hdr = enabled;
    }

    pub fn set_fanout(&self, value: u32) {
        self.state.lock().fanout = Some(value);
    }

    pub fn bind_raw(&self, addr: UserConstPtr<sockaddr>, addrlen: socklen_t) -> AxResult<()> {
        if addrlen as usize != size_of::<SockAddrLl>() {
            return Err(AxError::InvalidInput);
        }
        let addr = addr.cast::<SockAddrLl>().get_as_ref()?;
        let mut state = self.state.lock();
        state.ifindex = addr.sll_ifindex;
        Ok(())
    }

    pub fn send_raw(
        &self,
        len: usize,
        addr: UserConstPtr<sockaddr>,
        addrlen: socklen_t,
    ) -> AxResult<usize> {
        if !addr.is_null() {
            if addrlen as usize != size_of::<SockAddrLl>() {
                return Err(AxError::InvalidInput);
            }
            let addr = addr.cast::<SockAddrLl>().get_as_ref()?;
            if addr.sll_ifindex < 0 {
                return Err(AxError::InvalidInput);
            }
        }
        Ok(len)
    }

    pub fn set_rx_ring(&self, req: TpacketReq3) -> AxResult<()> {
        let mut state = self.state.lock();

        if req.tp_block_nr == 0 {
            if req.tp_frame_nr != 0 {
                return Err(AxError::InvalidInput);
            }
            state.rx_ring = None;
            return Ok(());
        }

        if req.tp_block_size == 0 || req.tp_frame_size == 0 || req.tp_frame_nr == 0 {
            return Err(AxError::InvalidInput);
        }

        if state.rx_ring.is_some() {
            return Err(AxError::ResourceBusy);
        }

        if !req.tp_block_size.is_multiple_of(PAGE_SIZE_4K as u32)
            || !req.tp_frame_size.is_multiple_of(TPACKET_ALIGNMENT)
        {
            return Err(AxError::InvalidInput);
        }

        let Some(min_frame_size) = tpacket_header_len(state.version).checked_add(state.reserve)
        else {
            return Err(AxError::InvalidInput);
        };
        if req.tp_frame_size < min_frame_size {
            return Err(AxError::InvalidInput);
        }

        if state.version == TPACKET_V3
            && align_u64(req.tp_sizeof_priv as u64, TPACKET3_PRIV_ALIGNMENT)
                .and_then(|priv_len| TPACKET3_BLOCK_HDRLEN.checked_add(priv_len))
                .and_then(|block_len| block_len.checked_add(min_frame_size as u64))
                .is_none_or(|need| need > req.tp_block_size as u64)
        {
            return Err(AxError::InvalidInput);
        }

        let frames_per_block = req.tp_block_size / req.tp_frame_size;
        if frames_per_block == 0 {
            return Err(AxError::InvalidInput);
        }

        let Some(total_frames) = frames_per_block.checked_mul(req.tp_block_nr) else {
            return Err(AxError::InvalidInput);
        };
        if total_frames != req.tp_frame_nr {
            return Err(AxError::InvalidInput);
        }

        state.rx_ring = Some(req);
        Ok(())
    }
}

impl FileLike for PacketSocket {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(Kstat {
            mode: S_IFSOCK | 0o777u32,
            ..Default::default()
        })
    }

    fn path(&self) -> Cow<'_, str> {
        alloc::format!("packet:[{}:{}]", self.protocol, self as *const _ as usize).into()
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Relaxed)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult<()> {
        self.nonblocking.store(nonblocking, Ordering::Relaxed);
        Ok(())
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        socket_ifreq_ioctl(cmd, arg)
    }
}

impl Pollable for PacketSocket {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

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

fn ifreq_index(ifr: &ifreq) -> Option<i32> {
    if ifreq_name_eq(ifr, b"lo") {
        Some(1)
    } else if ifreq_name_eq(ifr, b"eth0") {
        Some(2)
    } else {
        None
    }
}

fn ifreq_index_name(index: i32) -> Option<&'static [u8]> {
    match index {
        1 => Some(b"lo"),
        2 => Some(b"eth0"),
        _ => None,
    }
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

fn socket_ifconf_ioctl(arg: usize) -> AxResult<usize> {
    let ifc = UserPtr::<ifconf>::from(arg).get_as_mut()?;
    let entry_size = size_of::<ifreq>();
    let requested_len = ifc.ifc_len.max(0) as usize;
    let buf = unsafe { ifc.ifc_ifcu.ifcu_req };

    let written_len = if buf.is_null() {
        IFCONF_INTERFACES.len() * entry_size
    } else {
        let count = (requested_len / entry_size).min(IFCONF_INTERFACES.len());
        let dst = UserPtr::<ifreq>::from(buf as usize).get_as_mut_slice(count)?;
        for (slot, (name, ipv4)) in dst.iter_mut().zip(IFCONF_INTERFACES.iter().copied()) {
            *slot = make_ifconf_ifreq(name, ipv4);
        }
        count * entry_size
    };
    ifc.ifc_len = written_len as c_int;
    Ok(0)
}

pub fn socket_ifreq_ioctl(cmd: u32, arg: usize) -> AxResult<usize> {
    if cmd == SIOCGIFCONF {
        return socket_ifconf_ioctl(arg);
    }

    let ifr = UserPtr::<ifreq>::from(arg).get_as_mut()?;
    match cmd {
        SIOCGIFINDEX => {
            ifr.ifr_ifru.ifru_ivalue =
                ifreq_index(ifr).ok_or_else(|| AxError::from(LinuxError::ENODEV))?;
            Ok(0)
        }
        SIOCGIFNAME => {
            let index = unsafe { ifr.ifr_ifru.ifru_ivalue };
            let name = ifreq_index_name(index).ok_or_else(|| AxError::from(LinuxError::ENODEV))?;
            write_ifreq_name(ifr, name);
            Ok(0)
        }
        SIOCGIFFLAGS => {
            ifr.ifr_ifru.ifru_flags = (net_device_flags::IFF_UP as u32
                | net_device_flags::IFF_LOOPBACK as u32
                | net_device_flags::IFF_RUNNING as u32)
                as i16;
            Ok(0)
        }
        SIOCSIFFLAGS | SIOCSIFMTU => Ok(0),
        _ => Err(AxError::from(LinuxError::ENOTTY)),
    }
}
