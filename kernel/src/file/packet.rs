use alloc::borrow::Cow;
use core::{
    ffi::c_int,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::general::S_IFSOCK;
use memory_addr::PAGE_SIZE_4K;
use spin::Mutex;

use super::{FileHandle, FileLike, Kstat, get_typed_file};

pub const SOL_PACKET: u32 = 263;
pub const PACKET_RX_RING: u32 = 5;
pub const PACKET_VERSION: u32 = 10;
pub const PACKET_RESERVE: u32 = 12;

const TPACKET_V1: i32 = 0;
const TPACKET_V2: i32 = 1;
const TPACKET_V3: i32 = 2;
const TPACKET_ALIGNMENT: u32 = 16;
const TPACKET_HDRLEN: u32 = 52;
const TPACKET2_HDRLEN: u32 = 52;
const TPACKET3_HDRLEN: u32 = 68;
const TPACKET3_BLOCK_HDRLEN: u64 = 48;
const TPACKET3_PRIV_ALIGNMENT: u64 = 8;

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
    version: i32,
    reserve: u32,
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
                version: TPACKET_V1,
                reserve: 0,
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
}

impl Pollable for PacketSocket {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}
