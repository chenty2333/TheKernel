use alloc::borrow::Cow;
use core::{
    ffi::c_int,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::general::S_IFSOCK;
use spin::Mutex;

use super::{FileHandle, FileLike, Kstat, get_typed_file};

pub const SOL_PACKET: u32 = 263;
pub const PACKET_RX_RING: u32 = 5;
pub const PACKET_VERSION: u32 = 10;

const TPACKET_V1: i32 = 0;
const TPACKET_V2: i32 = 1;
const TPACKET_V3: i32 = 2;

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

pub struct PacketSocket {
    protocol: u16,
    version: AtomicU32,
    nonblocking: AtomicBool,
    rx_ring: Mutex<Option<TpacketReq3>>,
}

impl PacketSocket {
    pub fn new(protocol: u16) -> Self {
        Self {
            protocol,
            version: AtomicU32::new(TPACKET_V1 as u32),
            nonblocking: AtomicBool::new(false),
            rx_ring: Mutex::new(None),
        }
    }

    pub fn from_fd(fd: c_int) -> AxResult<FileHandle<Self>> {
        get_typed_file(fd)
    }

    pub fn set_packet_version(&self, version: i32) -> AxResult<()> {
        match version {
            TPACKET_V1 | TPACKET_V2 | TPACKET_V3 => {
                self.version.store(version as u32, Ordering::Relaxed);
                Ok(())
            }
            _ => Err(AxError::InvalidInput),
        }
    }

    pub fn set_rx_ring(&self, req: TpacketReq3) -> AxResult<()> {
        if req.tp_block_size == 0
            || req.tp_block_nr == 0
            || req.tp_frame_size == 0
            || req.tp_frame_nr == 0
        {
            return Err(AxError::InvalidInput);
        }

        if req.tp_sizeof_priv >= req.tp_block_size {
            return Err(AxError::InvalidInput);
        }

        if req.tp_frame_size > req.tp_block_size
            || !req.tp_block_size.is_multiple_of(req.tp_frame_size)
        {
            return Err(AxError::InvalidInput);
        }

        if req.tp_frame_nr < req.tp_block_nr {
            return Err(AxError::InvalidInput);
        }

        *self.rx_ring.lock() = Some(req);
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
