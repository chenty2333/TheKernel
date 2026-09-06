//! Linux AF_PACKET adapter over the generic bounded packet broker.
//!
//! This file owns only Layer 3 glue: namespace retention, lower-endpoint
//! publication, errno conversion, ordinary queue copies, and file readiness.
//! Linux value/state rules remain in `thekernel-linux-packet`; packet capture,
//! injection, queue budgets, and wake registration remain in `axnet-ng`.
//! TPACKET frame and block rings are backed by AX-owned shared pages and fanout selection
//! occurs in the broker before endpoint enqueue. Ordinary endpoint statistics
//! retain their single destructive owner in Layer 1.

use alloc::{borrow::Cow, boxed::Box, sync::Arc, vec::Vec};
use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    task::{Context, Poll},
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::{paging::PageSize, time::TimeValue};
use axio::prelude::*;
use axnet::{
    InterfaceKind,
    packet::{
        LinkHardwareType, LinkPacketType, MAX_PACKET_FRAME_BYTES, PacketAncillaryCapabilities,
        PacketDeviceCapabilities, PacketEndpoint, PacketError as PacketMechanismError,
        PacketFanout, PacketFanoutMode, PacketMetadata, PacketProtocol, PacketRingSink,
        PacketSelector, PacketSendRequest, PacketView as EndpointPacketView,
    },
};
use axpoll::{IoEvents, Pollable};
use axsync::{Mutex, MutexGuard};
use axtask::future::{TimerRegistrationError, sleep_until};
use thekernel_linux_packet::{
    FrameLayout, GetPacketOption, InterfaceIndex, LinkLayerAddress, LinkLayerInfo,
    PacketBindRequest, PacketBinding, PacketError, PacketOptionValue, PacketSendAddress,
    PacketSocketState, PacketSocketType, PacketStatistics, PacketType, ProtocolSelector,
    ReceiveFlags, SetPacketOption, SockAddrLl,
};
#[cfg(test)]
use thekernel_linux_packet::{SocketFilterAncillary, encoded_socket_filter_ancillary};

use super::{
    FileLike, FileMmapProtection, FileMmapRequest, FixedSharedMmapRegion, IoDst, IoSrc,
    IoctlContext, Kstat, PreparedFileMmap, PseudoInode, packet::socket_ifreq_ioctl,
    try_pseudo_inode_path,
};
use crate::{mm::SharedPages, readiness::block_on_poll_io, task::NetworkNamespace};

#[cfg(test)]
extern crate std;

const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;

/// Completed ordinary receive together with the address and truncation facts
/// needed by a later `recvfrom`/`recvmsg` userspace adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PacketReceiveResult {
    copied_len: usize,
    returned_len: usize,
    message_truncated: bool,
    address: SockAddrLl,
}

impl PacketReceiveResult {
    pub(crate) const fn copied_len(self) -> usize {
        self.copied_len
    }

    pub(crate) const fn returned_len(self) -> usize {
        self.returned_len
    }

    pub(crate) const fn message_truncated(self) -> bool {
        self.message_truncated
    }

    pub(crate) const fn address(self) -> SockAddrLl {
        self.address
    }
}

/// Kernel-owned admission for one ordinary packet submission.
///
/// This freezes only normalized device and link-layer facts. It intentionally
/// exposes neither a Linux UAPI layout nor a userspace pointer. The current
/// namespace has no device hotplug, so the lower device cannot disappear
/// between preparation and the single synchronous submit; a future hotplug
/// implementation must add an explicit device generation/revocation check.
pub(crate) struct PacketSendPlan {
    interface_index: u32,
    socket_type: PacketSocketType,
    protocol: u16,
    destination: [u8; 8],
    destination_len: usize,
    payload_len: usize,
}

/// One complete AF_PACKET transmit admission.  The legacy OUTPUT and lower
/// service locks are acquired once, before a TX-ring frame is cleared or a
/// userspace source is consumed, and are retained until submission.  This is
/// deliberately not a probe: a RWF_NOWAIT caller either owns both domains or
/// returns EAGAIN without changing socket/ring state.
struct PacketSendPermit<'a> {
    iptables: crate::syscall::IptablesOutputPermit<'a>,
    service: axnet::NetStackServicePermit<'a>,
    state: MutexGuard<'a, PacketSocketState>,
    ring_config: MutexGuard<'a, ()>,
    tx_ring: MutexGuard<'a, Option<Arc<PacketTxRing>>>,
}

impl PacketSendPermit<'_> {
    fn interfaces(&self) -> Vec<axnet::InterfaceInfo> {
        self.service.interfaces()
    }

    fn packet_device_capabilities(&self, interface_index: u32) -> Option<PacketDeviceCapabilities> {
        self.service.packet_device_capabilities(interface_index)
    }

    fn submit(
        &mut self,
        plan: &PacketSendPlan,
        origin: &PacketEndpoint,
        payload: &[u8],
    ) -> AxResult<()> {
        // `iptables` remains borrowed for the complete payload transaction.
        // Recheck from the retained table rather than taking a deep lock.
        self.iptables.verify()?;
        let request = match plan.socket_type {
            PacketSocketType::Raw => PacketSendRequest::Raw {
                protocol: plan.protocol,
                frame: payload,
            },
            PacketSocketType::Datagram => PacketSendRequest::Cooked {
                protocol: plan.protocol,
                destination: &plan.destination[..plan.destination_len],
                payload,
            },
        };
        self.service
            .send_packet(plan.interface_index, origin, request)
    }
}

/// One AF_PACKET open-file backend.
///
/// `state` is the authoritative Linux bind/option state. `endpoint` owns the
/// bounded lower queue and readiness source. Ordinary receive claims its queue
/// record before usercopy, while `MSG_PEEK` takes a retained clone; this matches
/// Linux's distinct EFAULT consumption behavior without an OFD-wide recv lock.
pub(crate) struct PacketSocket {
    net_ns: Arc<NetworkNamespace>,
    endpoint: Arc<PacketEndpoint>,
    state: Mutex<PacketSocketState>,
    filter_control: Mutex<PacketFilterControl>,
    nonblocking: AtomicBool,
    ring_config: Mutex<()>,
    version: Mutex<PacketVersion>,
    ring: Mutex<Option<Arc<PacketRxRing>>>,
    tx_ring: Mutex<Option<Arc<PacketTxRing>>>,
    mmap: Mutex<Option<Arc<PacketRingMmap>>>,
    mmap_published: AtomicBool,
    v3_timer: Mutex<Option<PacketV3Timer>>,
    inode: PseudoInode,
}

struct PacketV3Timer {
    deadline: TimeValue,
    future: Pin<Box<dyn Future<Output = Result<(), TimerRegistrationError>> + Send>>,
}

const TP_STATUS_KERNEL: u32 = 0;
const TP_STATUS_USER: u32 = 1;
const TP_STATUS_BLK_TMO: u32 = 1 << 5;
const TPACKET1_HEADER: usize = 32;
const TPACKET2_HEADER: usize = 32;
const TPACKET1_HDRLEN: usize = 52;
const TPACKET2_HDRLEN: usize = 52;
const TPACKET3_HEADER: usize = 48;
const TPACKET3_HDRLEN: usize = 68;
const TPACKET3_MAC_OFFSET: usize = 80;
const TPACKET_BLOCK_DESC: usize = 48;
const TPACKET_ALIGNMENT: usize = 16;

/// Native-endian, x86_64 UAPI layouts written directly into the shared map.
/// V3 transfers ownership through `block_status`, never through a per-frame
/// status word.
#[repr(C)]
struct TpacketBlockDesc {
    version: u32,
    offset_to_priv: u32,
    block_status: u32,
    num_pkts: u32,
    offset_to_first_pkt: u32,
    blk_len: u32,
    seq_num: u64,
    ts_first_pkt: [u8; 8],
    ts_last_pkt: [u8; 8],
}

#[repr(C)]
struct Tpacket3Hdr {
    next_offset: u32,
    sec: u32,
    nsec: u32,
    snaplen: u32,
    len: u32,
    status: u32,
    mac: u16,
    net: u16,
    hv1: [u8; 12],
    padding: [u8; 8],
}

const _: [(); TPACKET_BLOCK_DESC] = [(); core::mem::size_of::<TpacketBlockDesc>()];
const _: [(); TPACKET3_HEADER] = [(); core::mem::size_of::<Tpacket3Hdr>()];

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum PacketVersion {
    V1,
    V2,
    V3,
}

impl PacketVersion {
    pub(crate) const fn raw(self) -> i32 {
        match self {
            Self::V1 => 0,
            Self::V2 => 1,
            Self::V3 => 2,
        }
    }
    pub(crate) const fn header_len(self) -> usize {
        match self {
            Self::V1 => TPACKET1_HDRLEN,
            Self::V2 => TPACKET2_HDRLEN,
            Self::V3 => TPACKET3_HDRLEN,
        }
    }
    pub(crate) const fn decode(raw: i32) -> Option<Self> {
        match raw {
            0 => Some(Self::V1),
            1 => Some(Self::V2),
            2 => Some(Self::V3),
            _ => None,
        }
    }
}

/// Kernel mapping backing an endpoint-owned AX ring sink.  Frame ownership is
/// transferred by the first status word: userspace returns it to zero, and
/// ingress publishes `TP_STATUS_USER` only after every header/payload byte is
/// visible in the mapped shared pages.
struct PacketRingMmap {
    pages: Arc<SharedPages>,
    region: FixedSharedMmapRegion,
}
impl PacketRingMmap {
    fn try_new(bytes: usize) -> AxResult<Arc<Self>> {
        let pages = Arc::try_new(SharedPages::new_fixed(bytes, PageSize::Size4K)?)
            .map_err(|_| AxError::NoMemory)?;
        let region = FixedSharedMmapRegion::try_new(
            0,
            pages.clone(),
            FileMmapProtection::READ | FileMmapProtection::WRITE,
        )?;
        Arc::try_new(Self { pages, region }).map_err(|_| AxError::NoMemory)
    }
}
#[derive(Clone, Copy)]
pub(crate) struct PacketV3Request {
    pub(crate) block_size: usize,
    pub(crate) block_nr: u32,
    pub(crate) frame_size: usize,
    pub(crate) frame_nr: u32,
    pub(crate) retire_blk_tov_ms: u32,
    pub(crate) private_size: usize,
    pub(crate) fill_rxhash: bool,
    pub(crate) socket_type: PacketSocketType,
}

enum PacketRxRingKind {
    Frames {
        frame_size: usize,
        frame_nr: u32,
        version: PacketVersion,
        next: AtomicU32,
    },
    V3 {
        request: PacketV3Request,
        state: Mutex<PacketV3ProducerState>,
        sequence: AtomicU64,
        freeze_q_cnt: AtomicU32,
    },
}

struct PacketV3ProducerState {
    block: u32,
    packets: u32,
    next_offset: usize,
    last_packet: usize,
    opened_at_nanos: u64,
}

struct PacketRxRing {
    pages: Mutex<Arc<SharedPages>>,
    base: AtomicU32,
    kind: PacketRxRingKind,
}
impl PacketRxRing {
    fn try_new(frame_size: usize, frame_nr: u32, version: PacketVersion) -> AxResult<Arc<Self>> {
        let bytes = frame_size
            .checked_mul(frame_nr as usize)
            .ok_or(AxError::NoMemory)?;
        let pages = Arc::try_new(SharedPages::new_fixed(bytes, PageSize::Size4K)?)
            .map_err(|_| AxError::NoMemory)?;
        Arc::try_new(Self {
            pages: Mutex::new(pages),
            base: AtomicU32::new(0),
            kind: PacketRxRingKind::Frames {
                frame_size,
                frame_nr,
                version,
                next: AtomicU32::new(0),
            },
        })
        .map_err(|_| AxError::NoMemory)
    }
    fn try_new_v3(request: PacketV3Request) -> AxResult<Arc<Self>> {
        let bytes = request
            .block_size
            .checked_mul(request.block_nr as usize)
            .ok_or(AxError::NoMemory)?;
        let pages = Arc::try_new(SharedPages::new_fixed(bytes, PageSize::Size4K)?)
            .map_err(|_| AxError::NoMemory)?;
        Arc::try_new(Self {
            pages: Mutex::new(pages),
            base: AtomicU32::new(0),
            kind: PacketRxRingKind::V3 {
                request,
                state: Mutex::new(PacketV3ProducerState {
                    block: 0,
                    packets: 0,
                    next_offset: 0,
                    last_packet: 0,
                    opened_at_nanos: 0,
                }),
                sequence: AtomicU64::new(1),
                freeze_q_cnt: AtomicU32::new(0),
            },
        })
        .map_err(|_| AxError::NoMemory)
    }
    fn replace_backing(&self, pages: Arc<SharedPages>, base: usize) {
        *self.pages.lock() = pages;
        self.base.store(base as u32, Ordering::Release)
    }
    fn mapping_len(&self) -> AxResult<usize> {
        match &self.kind {
            PacketRxRingKind::Frames {
                frame_size,
                frame_nr,
                ..
            } => frame_size
                .checked_mul(*frame_nr as usize)
                .ok_or(AxError::NoMemory),
            PacketRxRingKind::V3 { request, .. } => request
                .block_size
                .checked_mul(request.block_nr as usize)
                .ok_or(AxError::NoMemory),
        }
    }
    fn take_freeze_q_cnt(&self) -> u32 {
        match &self.kind {
            PacketRxRingKind::V3 { freeze_q_cnt, .. } => freeze_q_cnt.swap(0, Ordering::AcqRel),
            PacketRxRingKind::Frames { .. } => 0,
        }
    }
    fn v3_deadline(&self) -> Option<u64> {
        match &self.kind {
            PacketRxRingKind::V3 { request, state, .. } if request.retire_blk_tov_ms != 0 => {
                let state = state.lock();
                (state.packets != 0).then_some(
                    state
                        .opened_at_nanos
                        .saturating_add(u64::from(request.retire_blk_tov_ms) * 1_000_000),
                )
            }
            _ => None,
        }
    }
}
struct PacketTxRing {
    pages: Mutex<Arc<SharedPages>>,
    base: AtomicU32,
    frame_size: usize,
    frame_nr: u32,
    version: PacketVersion,
    next: AtomicU32,
}
impl PacketTxRing {
    fn try_new(frame_size: usize, frame_nr: u32, version: PacketVersion) -> AxResult<Arc<Self>> {
        let bytes = frame_size
            .checked_mul(frame_nr as usize)
            .ok_or(AxError::NoMemory)?;
        let pages = Arc::try_new(SharedPages::new_fixed(bytes, PageSize::Size4K)?)
            .map_err(|_| AxError::NoMemory)?;
        Arc::try_new(Self {
            pages: Mutex::new(pages),
            base: AtomicU32::new(0),
            frame_size,
            frame_nr,
            version,
            next: AtomicU32::new(0),
        })
        .map_err(|_| AxError::NoMemory)
    }
    fn replace_backing(&self, pages: Arc<SharedPages>, base: usize) {
        *self.pages.lock() = pages;
        self.base.store(base as u32, Ordering::Release)
    }
}
impl PacketRingSink for PacketRxRing {
    fn publish(&self, metadata: PacketMetadata, bytes: &[u8]) -> AxResult<bool> {
        match &self.kind {
            PacketRxRingKind::Frames {
                frame_size,
                frame_nr,
                version,
                next,
            } => {
                let header_len = version.header_len();
                if bytes.len() > frame_size.saturating_sub(header_len) {
                    return Ok(false);
                }
                let slot = next.load(Ordering::Acquire) % frame_nr;
                let offset =
                    self.base.load(Ordering::Acquire) as usize + slot as usize * frame_size;
                let pages = self.pages.lock().clone();
                let free = match version {
                    PacketVersion::V1 => {
                        let mut status = [0; 8];
                        pages.read_bytes(offset, &mut status)?;
                        u64::from_ne_bytes(status) == TP_STATUS_KERNEL as u64
                    }
                    PacketVersion::V2 => {
                        let mut status = [0; 4];
                        pages.read_bytes(offset, &mut status)?;
                        u32::from_ne_bytes(status) == TP_STATUS_KERNEL
                    }
                    PacketVersion::V3 => unreachable!(),
                };
                if !free {
                    return Ok(false);
                }
                let mut header = [0u8; TPACKET2_HEADER];
                match version {
                    PacketVersion::V1 => {
                        header[8..12].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
                        header[12..16].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
                        header[16..18].copy_from_slice(&(header_len as u16).to_ne_bytes());
                        header[18..20].copy_from_slice(
                            &((header_len + usize::from(metadata.link_header_len)) as u16)
                                .to_ne_bytes(),
                        )
                    }
                    PacketVersion::V2 => {
                        header[4..8].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
                        header[8..12].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
                        header[12..14].copy_from_slice(&(header_len as u16).to_ne_bytes());
                        header[14..16].copy_from_slice(
                            &((header_len + usize::from(metadata.link_header_len)) as u16)
                                .to_ne_bytes(),
                        )
                    }
                    PacketVersion::V3 => unreachable!(),
                };
                let mut address = [0u8; 20];
                address[..2].copy_from_slice(&(linux_raw_sys::net::AF_PACKET as u16).to_ne_bytes());
                address[2..4].copy_from_slice(&metadata.protocol.to_be_bytes());
                address[4..8].copy_from_slice(&(metadata.interface_index as i32).to_ne_bytes());
                address[8..10].copy_from_slice(&hardware_type(metadata).to_ne_bytes());
                address[10] = packet_type_raw(metadata.packet_type);
                address[11] = metadata.address_len;
                address[12..20].copy_from_slice(&metadata.address);
                pages.write_bytes(offset, &header)?;
                pages.write_bytes(offset + TPACKET1_HEADER, &address)?;
                pages.write_bytes(offset + header_len, bytes)?;
                core::sync::atomic::fence(Ordering::Release);
                match version {
                    PacketVersion::V1 => {
                        pages.write_bytes(offset, &(TP_STATUS_USER as u64).to_ne_bytes())?
                    }
                    PacketVersion::V2 => {
                        pages.write_bytes(offset, &TP_STATUS_USER.to_ne_bytes())?
                    }
                    PacketVersion::V3 => unreachable!(),
                };
                next.store(slot.wrapping_add(1), Ordering::Release);
                Ok(true)
            }
            PacketRxRingKind::V3 {
                request,
                state,
                sequence,
                freeze_q_cnt,
            } => self.publish_v3(*request, state, sequence, freeze_q_cnt, metadata, bytes),
        }
    }
    fn readable(&self) -> bool {
        match &self.kind {
            PacketRxRingKind::Frames {
                frame_size,
                frame_nr,
                version,
                ..
            } => {
                for slot in 0..*frame_nr {
                    let offset =
                        self.base.load(Ordering::Acquire) as usize + slot as usize * frame_size;
                    let readable = match version {
                        PacketVersion::V1 => {
                            let mut status = [0; 8];
                            self.pages.lock().read_bytes(offset, &mut status).is_ok()
                                && u64::from_ne_bytes(status) & TP_STATUS_USER as u64 != 0
                        }
                        PacketVersion::V2 => {
                            let mut status = [0; 4];
                            self.pages.lock().read_bytes(offset, &mut status).is_ok()
                                && u32::from_ne_bytes(status) & TP_STATUS_USER != 0
                        }
                        PacketVersion::V3 => unreachable!(),
                    };
                    if readable {
                        return true;
                    }
                }
            }
            PacketRxRingKind::V3 { request, state, .. } => {
                if self.retire_v3_if_due(*request, state).is_err() {
                    return false;
                }
                for block in 0..request.block_nr {
                    let offset = self.base.load(Ordering::Acquire) as usize
                        + block as usize * request.block_size
                        + 8;
                    let mut status = [0; 4];
                    if self.pages.lock().read_bytes(offset, &mut status).is_ok()
                        && u32::from_ne_bytes(status) & TP_STATUS_USER != 0
                    {
                        return true;
                    }
                }
            }
        }
        false
    }
}

impl PacketRxRing {
    fn publish_v3(
        &self,
        request: PacketV3Request,
        state: &Mutex<PacketV3ProducerState>,
        sequence: &AtomicU64,
        freeze_q_cnt: &AtomicU32,
        metadata: PacketMetadata,
        bytes: &[u8],
    ) -> AxResult<bool> {
        let mac_len = usize::from(metadata.link_header_len);
        let net_offset = align_tpacket(TPACKET3_HDRLEN + mac_len.max(16));
        let mac_offset = match request.socket_type {
            PacketSocketType::Raw => net_offset - mac_len,
            PacketSocketType::Datagram => net_offset,
        };
        let record_len = align_v3(
            mac_offset
                .checked_add(bytes.len())
                .ok_or(AxError::NoMemory)?,
        );
        if record_len
            > request.block_size.saturating_sub(
                align_v3(TPACKET_BLOCK_DESC)
                    .checked_add(align_v3(request.private_size))
                    .ok_or(AxError::NoMemory)?,
            )
        {
            return Ok(false);
        }
        let pages = self.pages.lock().clone();
        let base = self.base.load(Ordering::Acquire) as usize;
        let mut producer = state.lock();
        self.retire_v3_locked(request, &pages, base, &mut producer, true)?;
        for _ in 0..request.block_nr {
            let block_offset = base + producer.block as usize * request.block_size;
            let mut status = [0; 4];
            pages.read_bytes(block_offset + 8, &mut status)?;
            core::sync::atomic::fence(Ordering::Acquire);
            if u32::from_ne_bytes(status) == TP_STATUS_KERNEL {
                break;
            }
            producer.block = (producer.block + 1) % request.block_nr;
            producer.packets = 0;
            producer.next_offset = 0;
            producer.last_packet = 0;
            producer.opened_at_nanos = 0;
        }
        let block_offset = base + producer.block as usize * request.block_size;
        let mut status = [0; 4];
        pages.read_bytes(block_offset + 8, &mut status)?;
        if u32::from_ne_bytes(status) != TP_STATUS_KERNEL {
            freeze_q_cnt.fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        }
        if producer.packets == 0 {
            let first = align_v3(TPACKET_BLOCK_DESC)
                .checked_add(align_v3(request.private_size))
                .ok_or(AxError::NoMemory)?;
            let now = crate::time::wall_time_nanos();
            let mut descriptor = [0u8; TPACKET_BLOCK_DESC];
            descriptor[..4].copy_from_slice(&1u32.to_ne_bytes());
            descriptor[4..8].copy_from_slice(&(TPACKET_BLOCK_DESC as u32).to_ne_bytes());
            descriptor[16..20].copy_from_slice(&(first as u32).to_ne_bytes());
            descriptor[20..24].copy_from_slice(&(first as u32).to_ne_bytes());
            descriptor[24..32]
                .copy_from_slice(&sequence.fetch_add(1, Ordering::Relaxed).to_ne_bytes());
            descriptor[32..36].copy_from_slice(&((now / 1_000_000_000) as u32).to_ne_bytes());
            descriptor[36..40].copy_from_slice(&((now % 1_000_000_000) as u32).to_ne_bytes());
            pages.write_bytes(block_offset, &descriptor)?;
            producer.next_offset = first;
            producer.opened_at_nanos = axhal::time::monotonic_time_nanos();
        }
        if producer
            .next_offset
            .checked_add(record_len)
            .is_none_or(|end| end > request.block_size)
        {
            self.retire_v3_locked(request, &pages, base, &mut producer, false)?;
            drop(producer);
            return self.publish_v3(request, state, sequence, freeze_q_cnt, metadata, bytes);
        }
        let packet_offset = block_offset + producer.next_offset;
        let mut header = [0u8; TPACKET3_HEADER];
        let now = crate::time::wall_time_nanos();
        header[..4].copy_from_slice(&(record_len as u32).to_ne_bytes());
        header[4..8].copy_from_slice(&((now / 1_000_000_000) as u32).to_ne_bytes());
        header[8..12].copy_from_slice(&((now % 1_000_000_000) as u32).to_ne_bytes());
        header[12..16].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
        header[16..20].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
        header[20..24].copy_from_slice(&TP_STATUS_USER.to_ne_bytes());
        header[24..26].copy_from_slice(&(mac_offset as u16).to_ne_bytes());
        header[26..28].copy_from_slice(&(net_offset as u16).to_ne_bytes());
        if request.fill_rxhash {
            header[28..32].copy_from_slice(&packet_rxhash(metadata, bytes).to_ne_bytes())
        }
        let mut address = [0u8; 20];
        address[..2].copy_from_slice(&(linux_raw_sys::net::AF_PACKET as u16).to_ne_bytes());
        address[2..4].copy_from_slice(&metadata.protocol.to_be_bytes());
        address[4..8].copy_from_slice(&(metadata.interface_index as i32).to_ne_bytes());
        address[8..10].copy_from_slice(&hardware_type(metadata).to_ne_bytes());
        address[10] = packet_type_raw(metadata.packet_type);
        address[11] = metadata.address_len;
        address[12..20].copy_from_slice(&metadata.address);
        pages.write_bytes(packet_offset, &header)?;
        pages.write_bytes(packet_offset + TPACKET3_HEADER, &address)?;
        pages.write_bytes(packet_offset + mac_offset, bytes)?;
        producer.last_packet = producer.next_offset;
        producer.next_offset += record_len;
        producer.packets += 1;
        pages.write_bytes(block_offset + 12, &producer.packets.to_ne_bytes())?;
        pages.write_bytes(
            block_offset + 20,
            &(producer.next_offset as u32).to_ne_bytes(),
        )?;
        if producer
            .next_offset
            .checked_add(mac_offset)
            .is_none_or(|end| end > request.block_size)
        {
            self.retire_v3_locked(request, &pages, base, &mut producer, false)?
        }
        Ok(true)
    }
    fn retire_v3_if_due(
        &self,
        request: PacketV3Request,
        state: &Mutex<PacketV3ProducerState>,
    ) -> AxResult<()> {
        let pages = self.pages.lock().clone();
        let base = self.base.load(Ordering::Acquire) as usize;
        let mut state = state.lock();
        self.retire_v3_locked(request, &pages, base, &mut state, true)
    }
    fn retire_v3_locked(
        &self,
        request: PacketV3Request,
        pages: &Arc<SharedPages>,
        base: usize,
        state: &mut PacketV3ProducerState,
        timed: bool,
    ) -> AxResult<()> {
        if state.packets == 0 {
            return Ok(());
        }
        let due = timed
            && request.retire_blk_tov_ms != 0
            && axhal::time::monotonic_time_nanos().saturating_sub(state.opened_at_nanos)
                >= u64::from(request.retire_blk_tov_ms) * 1_000_000;
        if !timed || due {
            let offset = base + state.block as usize * request.block_size;
            let now = crate::time::wall_time_nanos();
            let status = TP_STATUS_USER | if due { TP_STATUS_BLK_TMO } else { 0 };
            pages.write_bytes(offset + state.last_packet, &0u32.to_ne_bytes())?;
            pages.write_bytes(offset + 40, &((now / 1_000_000_000) as u32).to_ne_bytes())?;
            pages.write_bytes(offset + 44, &((now % 1_000_000_000) as u32).to_ne_bytes())?;
            core::sync::atomic::fence(Ordering::Release);
            pages.write_bytes(offset + 8, &status.to_ne_bytes())?;
            state.block = (state.block + 1) % request.block_nr;
            state.packets = 0;
            state.next_offset = 0;
            state.last_packet = 0;
            state.opened_at_nanos = 0;
        }
        Ok(())
    }
}

const fn align_tpacket(value: usize) -> usize {
    (value + TPACKET_ALIGNMENT - 1) & !(TPACKET_ALIGNMENT - 1)
}
const fn align_v3(value: usize) -> usize {
    (value + 7) & !7
}
fn packet_rxhash(metadata: PacketMetadata, bytes: &[u8]) -> u32 {
    let mut hash = 2166136261u32 ^ (metadata.interface_index as u32);
    for byte in bytes {
        hash = (hash ^ u32::from(*byte)).wrapping_mul(16777619)
    }
    hash
}

#[derive(Default)]
struct PacketFilterControl {
    locked: bool,
    required_ancillary: PacketAncillaryCapabilities,
}

impl PacketFilterControl {
    /// Applies Linux's one-way `SO_LOCK_FILTER` transition.
    ///
    /// Zero is a no-op only while the filter is still unlocked. Once locked,
    /// a zero value is an attempted unlock and Linux rejects it with `EPERM`;
    /// repeated nonzero values remain idempotent.
    fn apply_lock(&mut self, value: i32) -> AxResult<()> {
        if self.locked && value == 0 {
            return Err(LinuxError::EPERM.into());
        }
        if value != 0 {
            self.locked = true;
        }
        Ok(())
    }
}

impl PacketSocket {
    /// Allocates a Linux state object and its namespace-local bounded endpoint
    /// before any descriptor becomes visible.
    pub(crate) fn try_new(
        socket_type: PacketSocketType,
        protocol: ProtocolSelector,
        net_ns: Arc<NetworkNamespace>,
    ) -> AxResult<Arc<Self>> {
        let state = PacketSocketState::new(socket_type, protocol);
        let endpoint = net_ns
            .stack()
            .subscribe_packets(selector_for_state(&state))
            .map_err(packet_mechanism_error)?;
        Arc::try_new(Self {
            net_ns,
            endpoint,
            state: Mutex::new(state),
            filter_control: Mutex::new(PacketFilterControl::default()),
            nonblocking: AtomicBool::new(false),
            ring_config: Mutex::new(()),
            version: Mutex::new(PacketVersion::V1),
            ring: Mutex::new(None),
            tx_ring: Mutex::new(None),
            mmap: Mutex::new(None),
            mmap_published: AtomicBool::new(false),
            v3_timer: Mutex::new(None),
            inode: PseudoInode::socket(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        &self.net_ns
    }

    pub(crate) fn binding(&self) -> PacketBinding {
        self.state.lock().binding()
    }

    /// Returns the immutable Linux packet socket type from the Layer 2 state
    /// core. Generic SOL_SOCKET introspection uses this typed value instead of
    /// downcasting through the unrelated network backend.
    pub(crate) fn socket_type(&self) -> PacketSocketType {
        self.state.lock().socket_type()
    }

    /// Returns the one-way SO_LOCK_FILTER state shared by all descriptors
    /// referring to this open file description.
    pub(crate) fn filter_locked(&self) -> bool {
        self.filter_control.lock().locked
    }

    /// Installs an already verified classic-BPF filter.  The control mutex
    /// serializes attach, detach, and the one-way lock with the endpoint
    /// transition, while the endpoint itself keeps retired filter owners out
    /// of its state lock.
    pub(crate) fn attach_filter(
        &self,
        filter: Arc<crate::packet_cbpf::PacketCbpfFilter>,
    ) -> AxResult<()> {
        let state = self.state.lock();
        let socket_type = state.socket_type();
        let binding = state.binding();
        let required_ancillary = filter.required_ancillary_capabilities();
        let mut control = self.filter_control.lock();
        if control.locked {
            return Err(LinuxError::EPERM.into());
        }
        validate_filter_capabilities(
            &self.net_ns,
            socket_type,
            binding.interface(),
            required_ancillary,
        )?;
        let publication = crate::packet_cbpf::try_reserve_published().ok_or(AxError::NoMemory)?;
        self.endpoint
            .set_filter(Some(filter))
            .map_err(packet_mechanism_error)?;
        // The endpoint publication above is the only fallible transition;
        // retain the same requirement snapshot for later bind validation.
        control.required_ancillary = required_ancillary;
        publication.commit();
        Ok(())
    }

    /// Detaches the current classic-BPF filter, preserving Linux's ENOENT
    /// result when no filter is attached.
    pub(crate) fn detach_filter(&self) -> AxResult<()> {
        let mut control = self.filter_control.lock();
        if control.locked {
            return Err(LinuxError::EPERM.into());
        }
        if !self.endpoint.filter_attached() {
            return Err(LinuxError::ENOENT.into());
        }
        let result = self
            .endpoint
            .set_filter(None)
            .map_err(packet_mechanism_error);
        if result.is_ok() {
            control.required_ancillary = PacketAncillaryCapabilities::NONE;
        }
        result
    }

    /// Applies the Linux `SO_LOCK_FILTER` value and its irreversible-unlock
    /// error semantics.
    pub(crate) fn lock_filter(&self, value: i32) -> AxResult<()> {
        self.filter_control.lock().apply_lock(value)
    }

    /// Publishes a bind as `validate device -> lower selector -> ABI state`.
    ///
    /// The state mutex excludes another adapter transition between prepare and
    /// publish. Therefore the final publication cannot become stale; if lower
    /// selector admission fails, live Linux state remains unchanged.
    pub(crate) fn bind(&self, request: PacketBindRequest) -> AxResult<()> {
        let mut state = self.state.lock();
        let plan = state.prepare_bind(request).map_err(packet_error)?;
        let replacement = plan.replacement();
        validate_receive_device(&self.net_ns, state.socket_type(), replacement.interface())?;
        let control = self.filter_control.lock();
        validate_filter_capabilities(
            &self.net_ns,
            state.socket_type(),
            replacement.interface(),
            control.required_ancillary,
        )?;

        if !plan.is_noop() {
            let selector =
                selector_for_binding(state.socket_type(), replacement, state.ignore_outgoing());
            self.endpoint
                .set_selector(selector)
                .map_err(packet_mechanism_error)?;
        }

        state.publish_bind(plan).map_err(|error| {
            debug_assert_eq!(error, PacketError::StaleBindPlan);
            AxError::BadState
        })?;
        drop(control);
        Ok(())
    }

    /// Returns a coherent name from the live binding and a matching interface
    /// snapshot. No userspace pointer or device reference escapes this method.
    pub(crate) fn get_name(&self) -> AxResult<SockAddrLl> {
        let state = self.state.lock();
        let interface = state.binding().interface();
        let link = if interface.is_any() {
            None
        } else {
            let raw = exact_interface(interface)?;
            let info = self
                .net_ns
                .stack()
                .interfaces()
                .into_iter()
                .find(|candidate| candidate.index == raw)
                .ok_or(AxError::NoSuchDevice)?;
            let mut bytes = [0_u8; 8];
            let address_len = match info.hardware_address {
                Some(address) => {
                    bytes[..address.len()].copy_from_slice(&address);
                    address.len() as u8
                }
                None => 0,
            };
            let address = LinkLayerAddress::new(bytes, address_len).map_err(packet_error)?;
            Some(
                LinkLayerInfo::new(interface, hardware_type_for_kind(info.kind), address)
                    .map_err(packet_error)?,
            )
        };
        state.get_name(link).map_err(packet_error)
    }

    /// Applies a decoded ordinary packet option as `lower selector -> state`.
    /// A failed selector epoch allocation leaves the Linux-visible value
    /// unchanged.
    pub(crate) fn set_packet_option(&self, option: SetPacketOption) -> AxResult<()> {
        let mut state = self.state.lock();
        let SetPacketOption::IgnoreOutgoing(enabled) = option;
        if state.ignore_outgoing() == enabled {
            return Ok(());
        }
        let selector = selector_for_binding(state.socket_type(), state.binding(), enabled);
        self.endpoint
            .set_selector(selector)
            .map_err(packet_mechanism_error)?;
        state.set_option(option);
        Ok(())
    }

    fn prepare_ring_mapping(
        rx: Option<&Arc<PacketRxRing>>,
        tx: Option<&Arc<PacketTxRing>>,
    ) -> AxResult<Option<Arc<PacketRingMmap>>> {
        let rx_len = match rx {
            Some(ring) => ring.mapping_len()?,
            None => 0,
        };
        let tx_len = match tx {
            Some(ring) => ring
                .frame_size
                .checked_mul(ring.frame_nr as usize)
                .ok_or(AxError::NoMemory)?,
            None => 0,
        };
        let total = rx_len.checked_add(tx_len).ok_or(AxError::NoMemory)?;
        if total == 0 {
            return Ok(None);
        }
        PacketRingMmap::try_new(total).map(Some)
    }

    fn ring_header_len(&self) -> usize {
        self.version.lock().header_len()
    }

    /// Setting PACKET_VERSION is an OFD transition. Linux forbids changing
    /// layout after either ring exists, so a failed transition leaves the
    /// default V1 state untouched.
    pub(crate) fn set_packet_version(&self, raw: i32) -> AxResult<()> {
        let version = PacketVersion::decode(raw).ok_or(AxError::InvalidInput)?;
        let _serial = self.ring_config.lock();
        if self.ring.lock().is_some() || self.tx_ring.lock().is_some() {
            return Err(AxError::ResourceBusy);
        }
        *self.version.lock() = version;
        Ok(())
    }

    pub(crate) fn packet_version(&self) -> PacketVersion {
        *self.version.lock()
    }

    pub(crate) fn take_v3_freeze_q_cnt(&self) -> u32 {
        self.ring
            .lock()
            .as_ref()
            .map_or(0, |ring| ring.take_freeze_q_cnt())
    }

    fn arm_v3_timer(&self, context: &mut Context<'_>) -> Result<(), axpoll::PollRegistrationError> {
        let deadline = self
            .ring
            .lock()
            .as_ref()
            .and_then(|ring| ring.v3_deadline());
        let Some(deadline) = deadline else {
            *self.v3_timer.lock() = None;
            return Ok(());
        };
        let deadline = TimeValue::from_nanos(deadline);
        let mut timer = self.v3_timer.lock();
        if timer
            .as_ref()
            .is_none_or(|timer| timer.deadline != deadline)
        {
            let future = Box::try_new(sleep_until(deadline))
                .map_err(|_| axpoll::PollRegistrationError::NoMemory)?;
            *timer = Some(PacketV3Timer {
                deadline,
                future: Box::into_pin(future),
            });
        }
        let expired = timer
            .as_mut()
            .is_some_and(|timer| matches!(timer.future.as_mut().poll(context), Poll::Ready(_)));
        if expired {
            *timer = None;
            context.waker().wake_by_ref();
        }
        Ok(())
    }

    /// Prepare pages and a complete mmap object before publishing either the
    /// endpoint sink or Linux-visible ring state. All-zero tpacket_req is the
    /// Linux teardown operation; it atomically removes the requested half
    /// before any later configuration can observe it.
    pub(crate) fn configure_rx_ring(&self, frame_size: usize, frame_nr: u32) -> AxResult<()> {
        let _serial = self.ring_config.lock();
        if self.mmap_published.load(Ordering::Acquire) {
            return Err(AxError::ResourceBusy);
        }
        if frame_size == 0 && frame_nr == 0 {
            *self.v3_timer.lock() = None;
            let tx = self.tx_ring.lock().as_ref().cloned();
            let mapping = Self::prepare_ring_mapping(None, tx.as_ref())?;
            self.endpoint
                .set_ring_sink(None)
                .map_err(packet_mechanism_error)?;
            *self.ring.lock() = None;
            *self.mmap.lock() = mapping;
            return Ok(());
        }
        if self.packet_version() == PacketVersion::V3 {
            return Err(AxError::InvalidInput);
        }
        let header_len = self.ring_header_len();
        if frame_size < header_len
            || !frame_size.is_multiple_of(16)
            || frame_nr == 0
            || self.ring.lock().is_some()
        {
            return Err(AxError::InvalidInput);
        }
        let ring = PacketRxRing::try_new(frame_size, frame_nr, *self.version.lock())?;
        let tx = self.tx_ring.lock().as_ref().cloned();
        let mapping = Self::prepare_ring_mapping(Some(&ring), tx.as_ref())?;
        if let Some(mapping) = mapping.as_ref() {
            ring.replace_backing(mapping.pages.clone(), 0);
            if let Some(tx) = tx.as_ref() {
                tx.replace_backing(mapping.pages.clone(), ring.mapping_len()?);
            }
        }
        self.endpoint
            .set_ring_sink(Some(ring.clone()))
            .map_err(packet_mechanism_error)?;
        *self.ring.lock() = Some(ring);
        *self.mmap.lock() = mapping;
        Ok(())
    }

    pub(crate) fn configure_rx_ring_v3(&self, request: PacketV3Request) -> AxResult<()> {
        let _serial = self.ring_config.lock();
        if self.mmap_published.load(Ordering::Acquire)
            || self.packet_version() != PacketVersion::V3
            || self.ring.lock().is_some()
        {
            return Err(AxError::ResourceBusy);
        }
        let ring = PacketRxRing::try_new_v3(request)?;
        *self.v3_timer.lock() = None;
        let tx = self.tx_ring.lock().as_ref().cloned();
        let mapping = Self::prepare_ring_mapping(Some(&ring), tx.as_ref())?;
        if let Some(mapping) = mapping.as_ref() {
            ring.replace_backing(mapping.pages.clone(), 0);
            if let Some(tx) = tx.as_ref() {
                tx.replace_backing(mapping.pages.clone(), ring.mapping_len()?);
            }
        }
        self.endpoint
            .set_ring_sink(Some(ring.clone()))
            .map_err(packet_mechanism_error)?;
        *self.ring.lock() = Some(ring);
        *self.mmap.lock() = mapping;
        Ok(())
    }

    pub(crate) fn configure_tx_ring(&self, frame_size: usize, frame_nr: u32) -> AxResult<()> {
        let _serial = self.ring_config.lock();
        if self.mmap_published.load(Ordering::Acquire) {
            return Err(AxError::ResourceBusy);
        }
        if frame_size == 0 && frame_nr == 0 {
            let rx = self.ring.lock().as_ref().cloned();
            let mapping = Self::prepare_ring_mapping(rx.as_ref(), None)?;
            *self.tx_ring.lock() = None;
            *self.mmap.lock() = mapping;
            return Ok(());
        }
        if self.packet_version() == PacketVersion::V3 {
            return Err(AxError::InvalidInput);
        }
        let header_len = self.ring_header_len();
        if frame_size < header_len
            || !frame_size.is_multiple_of(16)
            || frame_nr == 0
            || self.tx_ring.lock().is_some()
        {
            return Err(AxError::InvalidInput);
        }
        let ring = PacketTxRing::try_new(frame_size, frame_nr, *self.version.lock())?;
        let rx = self.ring.lock().as_ref().cloned();
        let mapping = Self::prepare_ring_mapping(rx.as_ref(), Some(&ring))?;
        if let Some(mapping) = mapping.as_ref() {
            if let Some(rx) = rx.as_ref() {
                self.endpoint
                    .set_ring_sink(None)
                    .map_err(packet_mechanism_error)?;
                rx.replace_backing(mapping.pages.clone(), 0);
            }
            let rx_len = match rx.as_ref() {
                Some(rx) => rx.mapping_len()?,
                None => 0,
            };
            ring.replace_backing(mapping.pages.clone(), rx_len);
            if let Some(rx) = rx.as_ref() {
                self.endpoint
                    .set_ring_sink(Some(rx.clone()))
                    .map_err(packet_mechanism_error)?;
            }
        }
        *self.tx_ring.lock() = Some(ring);
        *self.mmap.lock() = mapping;
        Ok(())
    }

    /// Associates this endpoint with an AF_PACKET fanout group.  The lower
    /// broker chooses only among members whose current selector matched the
    /// capture, preserving one-copy semantics without a packet-side clone.
    pub(crate) fn configure_fanout(&self, group: u16, kind: u16) -> AxResult<()> {
        let raw_mode = kind & 0xff;
        let flags = kind & !0xff;
        // DEFRAG is meaningful at the lower IP reassembly layer; the packet
        // broker receives completed link frames, so retaining it is a real
        // no-op rather than silently reclassifying group membership.
        if flags & !0x8000 != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        let mode = match raw_mode {
            0 => PacketFanoutMode::Hash,
            1 => PacketFanoutMode::LoadBalance,
            2 => PacketFanoutMode::Cpu,
            3 => PacketFanoutMode::Rollover,
            4 => PacketFanoutMode::Random,
            _ => return Err(LinuxError::EINVAL.into()),
        };
        self.endpoint
            .set_fanout(Some(PacketFanout { group, mode, flags }))
            .map_err(packet_mechanism_error)
    }

    pub(crate) fn flush_tx_ring(&self) -> AxResult<()> {
        self.flush_tx_ring_with_nonblocking(false)
    }
    fn flush_tx_ring_with_nonblocking(&self, nonblocking: bool) -> AxResult<()> {
        let ring = if nonblocking {
            self.tx_ring.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            self.tx_ring.lock()
        }
        .as_ref()
        .cloned();
        let Some(ring) = ring else { return Ok(()) };
        let pages = if nonblocking {
            ring.pages.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            ring.pages.lock()
        }
        .clone();
        let base = ring.base.load(Ordering::Acquire) as usize;
        for _ in 0..ring.frame_nr {
            let slot = ring.next.load(Ordering::Acquire) % ring.frame_nr;
            let offset = base + slot as usize * ring.frame_size;
            let mut header = [0u8; TPACKET2_HEADER];
            pages.read_bytes(offset, &mut header)?;
            let (status, len, mac) = match ring.version {
                PacketVersion::V1 => (
                    u64::from_ne_bytes(header[..8].try_into().unwrap()),
                    u32::from_ne_bytes(header[8..12].try_into().unwrap()) as usize,
                    u16::from_ne_bytes(header[16..18].try_into().unwrap()) as usize,
                ),
                PacketVersion::V2 => (
                    u32::from_ne_bytes(header[..4].try_into().unwrap()) as u64,
                    u32::from_ne_bytes(header[4..8].try_into().unwrap()) as usize,
                    u16::from_ne_bytes(header[12..14].try_into().unwrap()) as usize,
                ),
                PacketVersion::V3 => unreachable!(),
            };
            if status != 1 {
                break;
            }
            let clear = || match ring.version {
                PacketVersion::V1 => pages.write_bytes(offset, &0u64.to_ne_bytes()),
                PacketVersion::V2 => pages.write_bytes(offset, &0u32.to_ne_bytes()),
                PacketVersion::V3 => unreachable!(),
            };
            if mac > ring.frame_size || len > ring.frame_size.saturating_sub(mac) {
                clear()?;
                ring.next.store(slot.wrapping_add(1), Ordering::Release);
                continue;
            }
            let mut frame = Vec::new();
            frame
                .try_reserve_exact(len)
                .map_err(|_| AxError::NoMemory)?;
            frame.resize(len, 0);
            pages.read_bytes(offset + mac, &mut frame)?;
            let plan = self.prepare_send_with_nonblocking(frame.len(), None, nonblocking)?;
            self.send_prepared_with_nonblocking(plan, &frame, nonblocking)?;
            clear()?;
            ring.next.store(slot.wrapping_add(1), Ordering::Release);
        }
        Ok(())
    }

    /// Flushes producer-owned TX_RING frames through a previously retained
    /// transmit permit.  No policy or network-service lock is reacquired in
    /// this path, so a NOWAIT EAGAIN always happens before ownership changes.
    fn flush_tx_ring_with_permit(&self, permit: &mut PacketSendPermit<'_>) -> AxResult<()> {
        // Keep configuration serialization live for the entire flush.
        let _config = &permit.ring_config;
        let ring = permit.tx_ring.as_ref().cloned();
        let Some(ring) = ring else {
            return Ok(());
        };
        let pages = ring.pages.try_lock().ok_or(AxError::WouldBlock)?.clone();
        let base = ring.base.load(Ordering::Acquire) as usize;
        for _ in 0..ring.frame_nr {
            let slot = ring.next.load(Ordering::Acquire) % ring.frame_nr;
            let offset = base + slot as usize * ring.frame_size;
            let mut header = [0u8; TPACKET2_HEADER];
            pages.read_bytes(offset, &mut header)?;
            let (status, len, mac) = match ring.version {
                PacketVersion::V1 => (
                    u64::from_ne_bytes(header[..8].try_into().unwrap()),
                    u32::from_ne_bytes(header[8..12].try_into().unwrap()) as usize,
                    u16::from_ne_bytes(header[16..18].try_into().unwrap()) as usize,
                ),
                PacketVersion::V2 => (
                    u32::from_ne_bytes(header[..4].try_into().unwrap()) as u64,
                    u32::from_ne_bytes(header[4..8].try_into().unwrap()) as usize,
                    u16::from_ne_bytes(header[12..14].try_into().unwrap()) as usize,
                ),
                PacketVersion::V3 => unreachable!(),
            };
            if status != 1 {
                break;
            }
            let clear = || match ring.version {
                PacketVersion::V1 => pages.write_bytes(offset, &0u64.to_ne_bytes()),
                PacketVersion::V2 => pages.write_bytes(offset, &0u32.to_ne_bytes()),
                PacketVersion::V3 => unreachable!(),
            };
            if mac > ring.frame_size || len > ring.frame_size.saturating_sub(mac) {
                clear()?;
                ring.next.store(slot.wrapping_add(1), Ordering::Release);
                continue;
            }
            let plan = self.prepare_send_with_permit(len, None, permit)?;
            let mut frame = Vec::new();
            frame
                .try_reserve_exact(len)
                .map_err(|_| AxError::NoMemory)?;
            frame.resize(len, 0);
            pages.read_bytes(offset + mac, &mut frame)?;
            permit.submit(&plan, self.endpoint.as_ref(), &frame)?;
            clear()?;
            ring.next.store(slot.wrapping_add(1), Ordering::Release);
        }
        Ok(())
    }

    /// Reads one decoded option. Statistics are taken and reset exactly once
    /// by the endpoint; neither this adapter nor Layer 2 owns a second reset.
    pub(crate) fn get_packet_option(&self, option: GetPacketOption) -> PacketOptionValue {
        match option {
            GetPacketOption::IgnoreOutgoing => {
                PacketOptionValue::IgnoreOutgoing(self.state.lock().ignore_outgoing())
            }
            GetPacketOption::Statistics => {
                let stats = self.endpoint.take_stats();
                PacketOptionValue::Statistics(PacketStatistics::from_destructive_snapshot(
                    stats.packets,
                    stats.drops,
                    stats.filter_rejected,
                    stats.filter_errors,
                ))
            }
        }
    }

    /// Performs one ordinary queue receive. `nonblocking` is an OFD snapshot;
    /// `MSG_DONTWAIT` remains a syscall-layer override and is folded into that
    /// value by the future `recvmsg` adapter.
    pub(crate) fn recv_with_nonblocking(
        &self,
        dst: &mut IoDst,
        flags: ReceiveFlags,
        nonblocking: bool,
    ) -> AxResult<PacketReceiveResult> {
        self.recv_with_operation_nonblocking(dst, flags, nonblocking, false)
    }

    pub(crate) fn recv_with_operation_nonblocking(
        &self,
        dst: &mut IoDst,
        flags: ReceiveFlags,
        nonblocking: bool,
        nowait: bool,
    ) -> AxResult<PacketReceiveResult> {
        block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            let peek = flags.contains(ReceiveFlags::PEEK);
            let state = if nowait {
                self.state.try_lock().ok_or(AxError::WouldBlock)?
            } else {
                self.state.lock()
            };
            let socket_type = state.socket_type();
            drop(state);
            // Linux ordinary packet receive dequeues before usercopy: EFAULT
            // consumes the record. MSG_PEEK clones the head and therefore
            // retains it across the same fault.
            let record = if nowait {
                let mut permit = self.net_ns.stack().try_acquire_packet_service()?;
                permit.receive_no_poll(self.endpoint.as_ref(), peek)?
            } else {
                self.net_ns
                    .stack()
                    .try_receive_packet(self.endpoint.as_ref(), peek)?
            };
            let metadata = record.metadata();
            let header_len = usize::from(metadata.link_header_len);
            let frame_len = match socket_type {
                PacketSocketType::Raw => record.wire_len(),
                PacketSocketType::Datagram => record
                    .wire_len()
                    .checked_add(header_len)
                    .ok_or(AxError::InvalidInput)?,
            };
            let view = FrameLayout::new(frame_len, header_len)
                .and_then(|layout| layout.captured_view(socket_type, record.data().len()))
                .map_err(packet_error)?;
            let decision = view.receive_decision(dst.remaining_mut(), flags);
            // Layer 1 must claim atomically in `try_receive`; a peek followed
            // by a separate destructive call would race another OFD reader.
            // Keep that atomic choice checked against the Layer 2 contract.
            debug_assert_eq!(decision.queue_disposition().claims_before_copy(), !peek);
            let copied = dst.write(&record.data()[..decision.copy_len()])?;
            if copied != decision.copy_len() {
                return Err(AxError::BadState);
            }

            Ok(PacketReceiveResult {
                copied_len: copied,
                returned_len: decision.returned_len(),
                message_truncated: decision.message_truncated(),
                address: address_from_metadata(metadata)?,
            })
        })
    }

    /// Reads one ordinary queued packet using the OFD status captured by the
    /// caller for this complete operation.
    pub(crate) fn read_with_nonblocking(
        &self,
        dst: &mut IoDst,
        nonblocking: bool,
    ) -> AxResult<usize> {
        self.recv_with_nonblocking(dst, ReceiveFlags::EMPTY, nonblocking)
            .map(PacketReceiveResult::returned_len)
    }

    pub(crate) fn read_with_operation_nonblocking(
        &self,
        dst: &mut IoDst,
        nonblocking: bool,
        nowait: bool,
    ) -> AxResult<usize> {
        self.recv_with_operation_nonblocking(dst, ReceiveFlags::EMPTY, nonblocking, nowait)
            .map(PacketReceiveResult::returned_len)
    }

    /// Packet transmit has no blocking admission path today. Keep the
    /// operation-local nonblocking decision explicit so RWF_NOWAIT never
    /// needs to alter the OFD's O_NONBLOCK state.
    pub(crate) fn write_with_nonblocking(
        &self,
        src: &mut IoSrc,
        nonblocking: bool,
    ) -> AxResult<usize> {
        self.write_with_operation_nonblocking(src, nonblocking, false)
    }

    pub(crate) fn write_with_operation_nonblocking(
        &self,
        src: &mut IoSrc,
        _nonblocking: bool,
        nowait: bool,
    ) -> AxResult<usize> {
        if nowait {
            // Every shared admission precedes source import.  In particular,
            // a source EFAULT cannot flush an earlier TX_RING record.
            let mut permit = self.try_acquire_send_permit()?;
            // Shared-page ring access can fault/materialize backing pages and
            // therefore has no non-sleeping reservation yet.  Refuse NOWAIT
            // while a TX ring is installed before touching the source; the
            // ordinary send boundary remains responsible for its flush.
            if permit.tx_ring.is_some() {
                return Err(AxError::WouldBlock);
            }
            let len = src.remaining();
            let plan = self.prepare_send_with_permit(len, None, &permit)?;
            let mut payload = Vec::new();
            payload
                .try_reserve_exact(len)
                .map_err(|_| AxError::NoMemory)?;
            payload.resize(len, 0);
            src.read_exact(&mut payload)?;
            self.flush_tx_ring_with_permit(&mut permit)?;
            let result = permit
                .submit(&plan, self.endpoint.as_ref(), &payload)
                .map(|_| len);
            drop(permit);
            self.net_ns.stack().finish_packet_submission(false);
            return result;
        }
        self.flush_tx_ring_with_nonblocking(nowait)?;
        let len = src.remaining();
        let plan = self.prepare_send_with_nonblocking(len, None, nowait)?;
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(len)
            .map_err(|_| AxError::NoMemory)?;
        payload.resize(len, 0);
        // One file write is one packet. A source that violates its advertised
        // remaining length must fail instead of publishing a truncated frame.
        src.read_exact(&mut payload)?;
        self.send_prepared_with_nonblocking(plan, &payload, nowait)
    }

    /// Prepares one ordinary send without touching the payload or submitting
    /// any lower-layer work.
    ///
    /// Device existence/capabilities, MTU, binding/default destination, and
    /// the effective protocol are resolved here. Syscall and file-write
    /// adapters must call this before copying payload bytes from userspace so
    /// Linux mechanism errors keep precedence over a later payload fault.
    pub(crate) fn prepare_send(
        &self,
        payload_len: usize,
        destination: Option<PacketSendAddress>,
    ) -> AxResult<PacketSendPlan> {
        self.prepare_send_with_nonblocking(payload_len, destination, false)
    }

    fn prepare_send_with_nonblocking(
        &self,
        payload_len: usize,
        destination: Option<PacketSendAddress>,
        nowait: bool,
    ) -> AxResult<PacketSendPlan> {
        let state = if nowait {
            self.state.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            self.state.lock()
        };
        let socket_type = state.socket_type();
        let binding = state.binding();
        drop(state);

        let selected_interface = destination
            .map(PacketSendAddress::interface)
            .unwrap_or(binding.interface());
        let interface_index =
            exact_interface(selected_interface).map_err(|_| AxError::from(LinuxError::ENXIO))?;
        let info = self
            .net_ns
            .stack()
            .interfaces()
            .into_iter()
            .find(|candidate| candidate.index == interface_index)
            .ok_or_else(|| AxError::from(LinuxError::ENXIO))?;
        let capabilities = self
            .net_ns
            .stack()
            .packet_device_capabilities(interface_index)
            .ok_or_else(|| AxError::from(LinuxError::ENXIO))?;

        match socket_type {
            PacketSocketType::Raw if !capabilities.raw_send => {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            PacketSocketType::Datagram if !capabilities.cooked_send => {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            _ => {}
        }
        let max_len = match socket_type {
            PacketSocketType::Raw => info
                .mtu
                .checked_add(usize::from(capabilities.link_header_len))
                .ok_or(AxError::InvalidInput)?,
            PacketSocketType::Datagram => info.mtu,
        };
        if payload_len > max_len || payload_len > MAX_PACKET_FRAME_BYTES {
            return Err(LinuxError::EMSGSIZE.into());
        }

        let protocol = destination
            .map(PacketSendAddress::protocol)
            .unwrap_or(binding.protocol())
            .host_order();
        let mut address = [0_u8; 8];
        let destination_len = match (socket_type, destination) {
            (PacketSocketType::Raw, _) => 0,
            (PacketSocketType::Datagram, Some(destination)) => {
                let link = destination
                    .address_for_device(capabilities.address_len)
                    .map_err(packet_error)?;
                address = link.padded_bytes();
                usize::from(link.len())
            }
            (PacketSocketType::Datagram, None) => {
                default_cooked_destination(&mut address, info.kind, capabilities)?
            }
        };

        Ok(PacketSendPlan {
            interface_index,
            socket_type,
            protocol,
            destination: address,
            destination_len,
            payload_len,
        })
    }

    fn prepare_send_with_permit(
        &self,
        payload_len: usize,
        destination: Option<PacketSendAddress>,
        permit: &PacketSendPermit<'_>,
    ) -> AxResult<PacketSendPlan> {
        let socket_type = permit.state.socket_type();
        let binding = permit.state.binding();
        let selected_interface = destination
            .map(PacketSendAddress::interface)
            .unwrap_or(binding.interface());
        let interface_index =
            exact_interface(selected_interface).map_err(|_| AxError::from(LinuxError::ENXIO))?;
        let info = permit
            .interfaces()
            .into_iter()
            .find(|candidate| candidate.index == interface_index)
            .ok_or_else(|| AxError::from(LinuxError::ENXIO))?;
        let capabilities = permit
            .packet_device_capabilities(interface_index)
            .ok_or_else(|| AxError::from(LinuxError::ENXIO))?;
        match socket_type {
            PacketSocketType::Raw if !capabilities.raw_send => {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            PacketSocketType::Datagram if !capabilities.cooked_send => {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            _ => {}
        }
        let max_len = match socket_type {
            PacketSocketType::Raw => info
                .mtu
                .checked_add(usize::from(capabilities.link_header_len))
                .ok_or(AxError::InvalidInput)?,
            PacketSocketType::Datagram => info.mtu,
        };
        if payload_len > max_len || payload_len > MAX_PACKET_FRAME_BYTES {
            return Err(LinuxError::EMSGSIZE.into());
        }
        let protocol = destination
            .map(PacketSendAddress::protocol)
            .unwrap_or(binding.protocol())
            .host_order();
        let mut address = [0_u8; 8];
        let destination_len = match (socket_type, destination) {
            (PacketSocketType::Raw, _) => 0,
            (PacketSocketType::Datagram, Some(destination)) => {
                let link = destination
                    .address_for_device(capabilities.address_len)
                    .map_err(packet_error)?;
                address = link.padded_bytes();
                usize::from(link.len())
            }
            (PacketSocketType::Datagram, None) => {
                default_cooked_destination(&mut address, info.kind, capabilities)?
            }
        };
        Ok(PacketSendPlan {
            interface_index,
            socket_type,
            protocol,
            destination: address,
            destination_len,
            payload_len,
        })
    }

    fn try_acquire_send_permit(&self) -> AxResult<PacketSendPermit<'_>> {
        // All fallible NOWAIT admissions complete before a TX ring frame or
        // caller source is touched.  The held state guard also prevents a
        // later `prepare_send` from reporting EAGAIN after ring ownership
        // has advanced.
        let iptables = crate::syscall::try_acquire_iptables_output_permit(&self.net_ns)?;
        let service = self.net_ns.stack().try_acquire_packet_service()?;
        let state = self.state.try_lock().ok_or(AxError::WouldBlock)?;
        // `configure_tx_ring` holds this same serialization guard.  Keeping
        // it until submit closes the absent-ring preflight gap.
        let ring_config = self.ring_config.try_lock().ok_or(AxError::WouldBlock)?;
        let tx_ring = self.tx_ring.try_lock().ok_or(AxError::WouldBlock)?;
        Ok(PacketSendPermit {
            iptables,
            service,
            state,
            ring_config,
            tx_ring,
        })
    }

    /// Submits one already-copied ordinary RAW frame or cooked DGRAM payload
    /// using a matching side-effect-free admission.
    ///
    /// Layer 1 does not yet expose device completion credits or writable
    /// admission readiness. Consequently blocking and nonblocking sends share
    /// this single attempt and a racing lower `WouldBlock` is returned as-is;
    /// ring transmission, retry, and deferred completion are outside this
    /// baseline.
    pub(crate) fn send_prepared(&self, plan: PacketSendPlan, payload: &[u8]) -> AxResult<usize> {
        self.send_prepared_with_nonblocking(plan, payload, false)
    }

    fn send_prepared_with_nonblocking(
        &self,
        plan: PacketSendPlan,
        payload: &[u8],
        nowait: bool,
    ) -> AxResult<usize> {
        if payload.len() != plan.payload_len {
            return Err(AxError::BadState);
        }
        if nowait {
            crate::syscall::iptables_output_verdict_nowait(&self.net_ns)?;
        } else {
            crate::syscall::iptables_output_verdict(&self.net_ns)?;
        }
        super::netlink::nft_output_verdict(&self.net_ns)?;
        let request = match plan.socket_type {
            PacketSocketType::Raw => PacketSendRequest::Raw {
                protocol: plan.protocol,
                frame: payload,
            },
            PacketSocketType::Datagram => PacketSendRequest::Cooked {
                protocol: plan.protocol,
                destination: &plan.destination[..plan.destination_len],
                payload,
            },
        };
        if nowait {
            self.net_ns.stack().send_packet_nowait(
                plan.interface_index,
                self.endpoint.as_ref(),
                request,
            )?;
        } else {
            self.net_ns.stack().send_packet(
                plan.interface_index,
                self.endpoint.as_ref(),
                request,
            )?;
        }
        Ok(payload.len())
    }
}

impl FileLike for PacketSocket {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.read_with_nonblocking(dst, self.nonblocking())
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.write_with_nonblocking(src, self.nonblocking())
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

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        try_pseudo_inode_path("socket", self.inode.inode())
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        socket_ifreq_ioctl(context, self.net_ns.stack(), cmd, arg)
    }

    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        // Serialize VMA publication with ring replacement/teardown. Without
        // this, mmap could retain the old shared-page object between prepare
        // and the endpoint's ring publication.
        let _serial = self.ring_config.lock();
        if let Some(mapping) = self.mmap.lock().as_ref().cloned() {
            let plan = mapping.region.prepare(request)?;
            if plan.is_some() {
                self.mmap_published.store(true, Ordering::Release);
            }
            return Ok(plan);
        }
        Err(LinuxError::EOPNOTSUPP.into())
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult<()> {
        self.nonblocking.store(nonblocking, Ordering::Release);
        Ok(())
    }
}

impl Pollable for PacketSocket {
    fn poll(&self) -> IoEvents {
        // READABLE is queue-backed. WRITABLE is deliberately optimistic until
        // Layer 1 grows a device completion-credit readiness contract.
        self.net_ns
            .stack()
            .poll_packet_endpoint(self.endpoint.as_ref())
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        self.arm_v3_timer(context)?;
        self.net_ns
            .stack()
            .register_packet_endpoint(self.endpoint.as_ref(), context, events)
    }
}

fn selector_for_state(state: &PacketSocketState) -> PacketSelector {
    selector_for_binding(
        state.socket_type(),
        state.binding(),
        state.ignore_outgoing(),
    )
}

fn selector_for_binding(
    socket_type: PacketSocketType,
    binding: PacketBinding,
    ignore_outgoing: bool,
) -> PacketSelector {
    let protocol = match binding.protocol() {
        ProtocolSelector::Disabled => PacketProtocol::Disabled,
        ProtocolSelector::All => PacketProtocol::All,
        ProtocolSelector::Exact(protocol) => PacketProtocol::Exact(protocol.host_order()),
    };
    let interface = match binding.interface() {
        InterfaceIndex::Any => None,
        exact => Some(exact.raw() as u32),
    };
    let view = match socket_type {
        PacketSocketType::Raw => EndpointPacketView::Raw,
        PacketSocketType::Datagram => EndpointPacketView::Cooked,
    };
    PacketSelector::new(
        protocol,
        interface,
        view,
        binding.protocol() == ProtocolSelector::All && !ignore_outgoing,
    )
}

/// Verifies that every currently eligible receive device for a binding can
/// define the ancillary fields used by an already verified filter. A wildcard
/// socket is checked against all current devices because the namespace has no
/// hotplug path; an exact socket is checked against its one interface.
fn validate_filter_capabilities(
    net_ns: &NetworkNamespace,
    socket_type: PacketSocketType,
    interface: InterfaceIndex,
    required: PacketAncillaryCapabilities,
) -> AxResult<()> {
    if required.is_empty() {
        return Ok(());
    }

    let stack = net_ns.stack();
    let check = |interface_index: u32| -> AxResult<()> {
        let capabilities = stack
            .packet_device_capabilities(interface_index)
            .ok_or(LinuxError::ENODEV)?;
        validate_one_filter_capability(socket_type, capabilities, required)
    };

    match interface {
        InterfaceIndex::Any => {
            for info in stack.interfaces() {
                check(info.index)?;
            }
            Ok(())
        }
        exact => check(exact_interface(exact)?),
    }
}

fn validate_one_filter_capability(
    socket_type: PacketSocketType,
    capabilities: PacketDeviceCapabilities,
    required: PacketAncillaryCapabilities,
) -> AxResult<()> {
    let receives = match socket_type {
        PacketSocketType::Raw => capabilities.raw_receive,
        PacketSocketType::Datagram => capabilities.cooked_receive,
    };
    if receives && !capabilities.ancillary.supports(required) {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    Ok(())
}

fn validate_receive_device(
    net_ns: &NetworkNamespace,
    socket_type: PacketSocketType,
    interface: InterfaceIndex,
) -> AxResult<()> {
    if interface.is_any() {
        return Ok(());
    }
    let capabilities = net_ns
        .stack()
        .packet_device_capabilities(exact_interface(interface)?)
        .ok_or(AxError::NoSuchDevice)?;
    match socket_type {
        PacketSocketType::Raw if capabilities.raw_receive => Ok(()),
        PacketSocketType::Datagram if capabilities.cooked_receive => Ok(()),
        _ => Err(LinuxError::EOPNOTSUPP.into()),
    }
}

fn exact_interface(interface: InterfaceIndex) -> AxResult<u32> {
    u32::try_from(interface.raw())
        .ok()
        .filter(|index| *index != 0)
        .ok_or(AxError::InvalidInput)
}

fn default_cooked_destination(
    output: &mut [u8; 8],
    kind: InterfaceKind,
    capabilities: PacketDeviceCapabilities,
) -> AxResult<usize> {
    let len = usize::from(capabilities.address_len);
    if len > output.len() {
        return Err(AxError::InvalidInput);
    }
    match kind {
        // Linux's loopback/NOARP header builder accepts a null destination and
        // writes zeros. Ordinary Ethernet requires an explicit destination;
        // silently substituting the local source MAC would change the frame.
        InterfaceKind::Loopback => {
            output[..len].fill(0);
            Ok(len)
        }
        InterfaceKind::Ethernet => Err(AxError::InvalidInput),
    }
}

const fn hardware_type_for_kind(kind: InterfaceKind) -> u16 {
    match kind {
        InterfaceKind::Loopback => ARPHRD_LOOPBACK,
        InterfaceKind::Ethernet => ARPHRD_ETHER,
    }
}

const fn hardware_type(metadata: PacketMetadata) -> u16 {
    match metadata.hardware_type {
        LinkHardwareType::Ethernet => ARPHRD_ETHER,
        LinkHardwareType::Loopback => ARPHRD_LOOPBACK,
    }
}

const fn packet_type(packet_type: LinkPacketType) -> PacketType {
    match packet_type {
        LinkPacketType::Host => PacketType::HOST,
        LinkPacketType::Broadcast => PacketType::BROADCAST,
        LinkPacketType::Multicast => PacketType::MULTICAST,
        LinkPacketType::OtherHost => PacketType::OTHER_HOST,
        LinkPacketType::Outgoing => PacketType::OUTGOING,
    }
}

const fn packet_type_raw(packet_type: LinkPacketType) -> u8 {
    match packet_type {
        LinkPacketType::Host => 0,
        LinkPacketType::Broadcast => 1,
        LinkPacketType::Multicast => 2,
        LinkPacketType::OtherHost => 3,
        LinkPacketType::Outgoing => 4,
    }
}

fn address_from_metadata(metadata: PacketMetadata) -> AxResult<SockAddrLl> {
    let interface = InterfaceIndex::exact(metadata.interface_index).map_err(packet_error)?;
    let address =
        LinkLayerAddress::new(metadata.address, metadata.address_len).map_err(packet_error)?;
    Ok(SockAddrLl::new(
        interface,
        ProtocolSelector::from_host_order(metadata.protocol),
        hardware_type(metadata),
        packet_type(metadata.packet_type),
        address,
    ))
}

pub(crate) fn packet_error(error: PacketError) -> AxError {
    match error {
        PacketError::UnsupportedSocketType => LinuxError::ESOCKTNOSUPPORT.into(),
        PacketError::InvalidAddressFamily => LinuxError::EAFNOSUPPORT.into(),
        PacketError::UnsupportedReceiveFlags
        | PacketError::UnknownPacketOption
        | PacketError::UnsupportedPacketOption { .. } => LinuxError::EOPNOTSUPP.into(),
        PacketError::MissingLinkLayerInfo => AxError::NoSuchDevice,
        PacketError::StaleBindPlan | PacketError::LinkLayerInfoMismatch => AxError::BadState,
        PacketError::BindGenerationExhausted => LinuxError::EOVERFLOW.into(),
        PacketError::InvalidExactProtocol
        | PacketError::InvalidInterfaceIndex
        | PacketError::InvalidHardwareAddressLength
        | PacketError::InvalidPacketOptionValue
        | PacketError::InvalidBindingGeneration
        | PacketError::InvalidFrameLayout
        | PacketError::InvalidCapturedLength => AxError::InvalidInput,
        _ => AxError::InvalidInput,
    }
}

/// Maps the Linux-agnostic bounded broker's typed mechanism failures at the
/// sole ABI ownership boundary. No lower `PacketError` is allowed to fall
/// through an incidental `AxError` conversion.
fn packet_mechanism_error(error: PacketMechanismError) -> AxError {
    match error {
        PacketMechanismError::Allocation => AxError::NoMemory,
        PacketMechanismError::InvalidInput => AxError::InvalidInput,
        // A live `PacketSocket` owns both its namespace and broker endpoint.
        // Reaching a detached broker here is therefore an internal lifecycle
        // violation, not an observable device-removal condition.
        PacketMechanismError::Detached => AxError::BadState,
        PacketMechanismError::SequenceExhausted => LinuxError::EOVERFLOW.into(),
        PacketMechanismError::Capacity(_) => LinuxError::ENOBUFS.into(),
    }
}

/// Serializes host tests that share the emulated primary-CPU current-task slot.
///
/// A module-local latch would only order packet tests against each other, so
/// this defers to the crate-wide bootstrap.
#[cfg(test)]
pub(crate) fn packet_test_context() -> std::sync::MutexGuard<'static, ()> {
    crate::test_support::scheduler_test_context()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::UserNamespace;

    struct FaultDst {
        remaining: usize,
    }

    struct ShortSrc {
        bytes: &'static [u8],
        offset: usize,
        advertised: usize,
    }

    #[test]
    fn filter_lock_is_one_way_with_linux_unlock_error() {
        let mut control = PacketFilterControl::default();
        assert_eq!(control.apply_lock(0), Ok(()));
        assert!(!control.locked);
        assert_eq!(control.apply_lock(1), Ok(()));
        assert!(control.locked);
        assert_eq!(control.apply_lock(-1), Ok(()));
        assert!(control.locked);
        assert_eq!(control.apply_lock(0), Err(LinuxError::EPERM.into()));
        assert!(control.locked);
    }

    impl Write for FaultDst {
        fn write(&mut self, _buf: &[u8]) -> AxResult<usize> {
            Err(AxError::BadAddress)
        }

        fn flush(&mut self) -> AxResult<()> {
            Ok(())
        }
    }

    impl axio::IoBuf for FaultDst {
        fn remaining(&self) -> usize { self.remaining }
    }

    impl IoBufMut for FaultDst {
        fn remaining_mut(&self) -> usize {
            self.remaining
        }
    }

    impl Read for ShortSrc {
        fn read(&mut self, output: &mut [u8]) -> AxResult<usize> {
            let source = &self.bytes[self.offset..];
            let copied = source.len().min(output.len());
            output[..copied].copy_from_slice(&source[..copied]);
            self.offset += copied;
            Ok(copied)
        }
    }

    impl IoBuf for ShortSrc {
        fn remaining(&self) -> usize {
            self.advertised
        }
    }

    fn namespace() -> Arc<NetworkNamespace> {
        NetworkNamespace::try_new_loopback_only(UserNamespace::try_new_root().unwrap()).unwrap()
    }

    fn loopback_send_address(protocol: ProtocolSelector) -> PacketSendAddress {
        PacketSendAddress::try_from_network_order_fields(
            protocol.to_network_order_u16(),
            1,
            6,
            [0; 8],
        )
        .unwrap()
    }

    fn raw_ipv4_frame() -> [u8; 34] {
        let mut frame = [0_u8; 34];
        frame[12..14].copy_from_slice(&0x0800_u16.to_be_bytes());
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&20_u16.to_be_bytes());
        frame[22] = 64;
        frame
    }

    fn submit(
        socket: &PacketSocket,
        payload: &[u8],
        destination: Option<PacketSendAddress>,
    ) -> AxResult<usize> {
        let plan = socket.prepare_send(payload.len(), destination)?;
        socket.send_prepared(plan, payload)
    }

    fn packet_capabilities(hardware_type: LinkHardwareType) -> PacketDeviceCapabilities {
        PacketDeviceCapabilities {
            hardware_type,
            raw_receive: true,
            raw_send: true,
            cooked_receive: true,
            cooked_send: true,
            link_header_len: 14,
            address_len: 6,
            ancillary: PacketAncillaryCapabilities::CANONICAL,
            checksum: axnet::packet::PacketChecksumContext::UNKNOWN,
        }
    }

    #[test]
    fn ancillary_filter_capability_is_checked_before_publication() {
        let mut capabilities = packet_capabilities(LinkHardwareType::Ethernet);
        capabilities.ancillary = PacketAncillaryCapabilities::NONE;
        assert_eq!(
            validate_one_filter_capability(
                PacketSocketType::Raw,
                capabilities,
                PacketAncillaryCapabilities::MARK,
            )
            .unwrap_err(),
            LinuxError::EOPNOTSUPP.into()
        );
        assert_eq!(
            validate_one_filter_capability(
                PacketSocketType::Raw,
                capabilities,
                PacketAncillaryCapabilities::NONE,
            ),
            Ok(())
        );
    }

    #[test]
    fn typed_packet_mechanism_errors_are_mapped_only_at_the_linux_adapter() {
        use axnet::packet::PacketCapacity;

        assert_eq!(
            packet_mechanism_error(PacketMechanismError::Allocation),
            AxError::NoMemory
        );
        assert_eq!(
            packet_mechanism_error(PacketMechanismError::InvalidInput),
            AxError::InvalidInput
        );
        assert_eq!(
            packet_mechanism_error(PacketMechanismError::Detached),
            AxError::BadState
        );
        assert_eq!(
            packet_mechanism_error(PacketMechanismError::SequenceExhausted),
            LinuxError::EOVERFLOW.into()
        );
        assert_eq!(
            packet_mechanism_error(PacketMechanismError::Capacity(
                PacketCapacity::EndpointRegistry,
            )),
            LinuxError::ENOBUFS.into()
        );
        assert_eq!(
            packet_mechanism_error(PacketMechanismError::Capacity(
                PacketCapacity::SelectorEpochs,
            )),
            LinuxError::ENOBUFS.into()
        );
    }

    #[test]
    fn cooked_null_destination_is_loopback_only_and_canonical_zero() {
        let mut loopback = [9_u8; 8];
        assert_eq!(
            default_cooked_destination(
                &mut loopback,
                InterfaceKind::Loopback,
                packet_capabilities(LinkHardwareType::Loopback),
            ),
            Ok(6)
        );
        assert_eq!(&loopback[..6], &[0; 6]);

        let mut ethernet = [0_u8; 8];
        assert_eq!(
            default_cooked_destination(
                &mut ethernet,
                InterfaceKind::Ethernet,
                packet_capabilities(LinkHardwareType::Ethernet),
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn explicit_send_address_never_inherits_zero_fields_or_declared_halen() {
        let _context = packet_test_context();
        let socket = PacketSocket::try_new(
            PacketSocketType::Datagram,
            ProtocolSelector::from_host_order(0x0800),
            namespace(),
        )
        .unwrap();
        let raw_address = [1, 2, 3, 4, 5, 6, 7, 8];
        let destination =
            PacketSendAddress::try_from_network_order_fields(0, 1, 0, raw_address).unwrap();
        let plan = socket.prepare_send(20, Some(destination)).unwrap();
        assert_eq!(plan.interface_index, 1);
        assert_eq!(plan.protocol, 0);
        assert_eq!(plan.destination_len, 6);
        assert_eq!(&plan.destination[..6], &raw_address[..6]);

        let wildcard =
            PacketSendAddress::try_from_network_order_fields(0, 0, 0, raw_address).unwrap();
        let error = match socket.prepare_send(20, Some(wildcard)) {
            Ok(_) => panic!("explicit wildcard unexpectedly inherited the bound interface"),
            Err(error) => error,
        };
        assert_eq!(error, LinuxError::ENXIO.into());
    }

    #[test]
    fn packet_options_update_selector_and_statistics_have_one_reset_owner() {
        let _context = packet_test_context();
        let net_ns = namespace();
        let receiver =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();

        assert_eq!(
            receiver.get_packet_option(GetPacketOption::IgnoreOutgoing),
            PacketOptionValue::IgnoreOutgoing(false)
        );
        receiver
            .set_packet_option(SetPacketOption::IgnoreOutgoing(true))
            .unwrap();
        assert_eq!(
            receiver.get_packet_option(GetPacketOption::IgnoreOutgoing),
            PacketOptionValue::IgnoreOutgoing(true)
        );

        submit(
            &sender,
            &raw_ipv4_frame(),
            Some(loopback_send_address(ProtocolSelector::from_host_order(
                0x0800,
            ))),
        )
        .unwrap();

        let PacketOptionValue::Statistics(first) =
            receiver.get_packet_option(GetPacketOption::Statistics)
        else {
            panic!("statistics query returned a different option value")
        };
        assert_eq!(first.packets(), 1);
        assert_eq!(first.drops(), 0);

        let PacketOptionValue::Statistics(second) =
            receiver.get_packet_option(GetPacketOption::Statistics)
        else {
            panic!("statistics query returned a different option value")
        };
        assert!(second.is_empty());
    }

    #[test]
    fn attached_cbpf_filter_observes_real_loopback_protocol_context() {
        let _context = packet_test_context();
        let net_ns = namespace();
        let receiver =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns).unwrap();

        // Accept only the protocol supplied by the loopback device's real
        // PacketDeviceContext. The ancillary snapshot is complete as well,
        // but this filter deliberately checks the hot skb->protocol value.
        let filter = crate::packet_cbpf::PacketCbpfFilter::try_new(alloc::vec![
            axcbpf::Instruction::statement(
                axcbpf::opcode::LD_W_ABS,
                encoded_socket_filter_ancillary(SocketFilterAncillary::Protocol),
            ),
            axcbpf::Instruction::jump(axcbpf::opcode::JMP_JEQ_K, 0x0800, 0, 1),
            axcbpf::Instruction::statement(axcbpf::opcode::RET_K, u32::MAX),
            axcbpf::Instruction::statement(axcbpf::opcode::RET_K, 0),
        ])
        .unwrap();
        receiver.attach_filter(filter).unwrap();

        submit(
            &sender,
            &raw_ipv4_frame(),
            Some(loopback_send_address(ProtocolSelector::from_host_order(
                0x0800,
            ))),
        )
        .unwrap();
        assert_eq!(receiver.endpoint.queue_usage().0, 2);
        assert_eq!(
            receiver
                .endpoint
                .try_receive(false)
                .unwrap()
                .metadata()
                .protocol,
            0x0800
        );
        assert_eq!(
            receiver
                .endpoint
                .try_receive(false)
                .unwrap()
                .metadata()
                .protocol,
            0x0800
        );

        // The same real socket chain must reject a different protocol without
        // confusing the packet bytes' EtherType with skb->protocol.
        let mut arp_frame = raw_ipv4_frame();
        arp_frame[12..14].copy_from_slice(&0x0806_u16.to_be_bytes());
        submit(
            &sender,
            &arp_frame,
            Some(loopback_send_address(ProtocolSelector::from_host_order(
                0x0806,
            ))),
        )
        .unwrap();
        assert_eq!(receiver.endpoint.queue_usage().0, 0);
    }

    #[test]
    fn attached_cbpf_filter_observes_canonical_ancillary_state() {
        let _context = packet_test_context();
        let net_ns = namespace();
        let receiver =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns).unwrap();

        // Current devices own the canonical values: mark=0, one queue=0,
        // and no hardware VLAN extraction. Attach must validate that
        // capability once, and delivery must observe the same values rather
        // than returning EOPNOTSUPP per packet. The inline VLAN bytes below
        // deliberately do not populate the skb VLAN sidecar.
        let filter = crate::packet_cbpf::PacketCbpfFilter::try_new(alloc::vec![
            axcbpf::Instruction::statement(
                axcbpf::opcode::LD_W_ABS,
                encoded_socket_filter_ancillary(SocketFilterAncillary::Mark),
            ),
            axcbpf::Instruction::jump(axcbpf::opcode::JMP_JEQ_K, 0, 1, 0),
            axcbpf::Instruction::statement(axcbpf::opcode::RET_K, 0),
            axcbpf::Instruction::statement(
                axcbpf::opcode::LD_W_ABS,
                encoded_socket_filter_ancillary(SocketFilterAncillary::Queue),
            ),
            axcbpf::Instruction::jump(axcbpf::opcode::JMP_JEQ_K, 0, 1, 0),
            axcbpf::Instruction::statement(axcbpf::opcode::RET_K, 0),
            axcbpf::Instruction::statement(
                axcbpf::opcode::LD_W_ABS,
                encoded_socket_filter_ancillary(SocketFilterAncillary::VlanTag),
            ),
            axcbpf::Instruction::jump(axcbpf::opcode::JMP_JEQ_K, 0, 1, 0),
            axcbpf::Instruction::statement(axcbpf::opcode::RET_K, 0),
            axcbpf::Instruction::statement(
                axcbpf::opcode::LD_W_ABS,
                encoded_socket_filter_ancillary(SocketFilterAncillary::VlanTagPresent),
            ),
            axcbpf::Instruction::jump(axcbpf::opcode::JMP_JEQ_K, 0, 1, 0),
            axcbpf::Instruction::statement(axcbpf::opcode::RET_K, 0),
            axcbpf::Instruction::statement(
                axcbpf::opcode::LD_W_ABS,
                encoded_socket_filter_ancillary(SocketFilterAncillary::VlanTpid),
            ),
            axcbpf::Instruction::jump(axcbpf::opcode::JMP_JEQ_K, 0, 1, 0),
            axcbpf::Instruction::statement(axcbpf::opcode::RET_K, 0),
            axcbpf::Instruction::statement(axcbpf::opcode::RET_K, u32::MAX),
        ])
        .unwrap();
        receiver.attach_filter(filter).unwrap();

        let mut inline_vlan = raw_ipv4_frame();
        inline_vlan[12..14].copy_from_slice(&0x8100_u16.to_be_bytes());
        inline_vlan[14..16].copy_from_slice(&0x0064_u16.to_be_bytes());
        inline_vlan[16..18].copy_from_slice(&0x0800_u16.to_be_bytes());
        submit(
            &sender,
            &inline_vlan,
            Some(loopback_send_address(ProtocolSelector::from_host_order(
                0x0800,
            ))),
        )
        .unwrap();
        assert_eq!(receiver.endpoint.queue_usage().0, 2);
    }

    #[test]
    fn namespace_lifetime_and_exact_bind_are_owned_by_the_adapter() {
        let _context = packet_test_context();
        let net_ns = namespace();
        let weak = Arc::downgrade(&net_ns);
        let socket = PacketSocket::try_new(
            PacketSocketType::Raw,
            ProtocolSelector::Disabled,
            net_ns.clone(),
        )
        .unwrap();
        drop(net_ns);
        assert!(weak.upgrade().is_some());

        let request = PacketBindRequest::new(
            InterfaceIndex::exact(1).unwrap(),
            ProtocolSelector::from_host_order(0x0800),
        );
        socket.bind(request).unwrap();
        assert_eq!(socket.binding().interface().raw(), 1);
        let name = socket.get_name().unwrap();
        assert_eq!(name.interface().raw(), 1);
        assert_eq!(name.protocol().host_order(), 0x0800);
        assert_eq!(name.hardware_type(), ARPHRD_LOOPBACK);
        assert_eq!(name.address().as_bytes(), &[0; 6]);

        drop(socket);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn linux_selector_exposes_outgoing_only_to_eth_p_all() {
        let _context = packet_test_context();
        let net_ns = namespace();
        let all =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();
        let exact = PacketSocket::try_new(
            PacketSocketType::Raw,
            ProtocolSelector::from_host_order(0x0800),
            net_ns.clone(),
        )
        .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();

        submit(
            &sender,
            &raw_ipv4_frame(),
            Some(loopback_send_address(ProtocolSelector::from_host_order(
                0x0800,
            ))),
        )
        .unwrap();

        assert_eq!(all.endpoint.queue_usage().0, 2);
        assert_eq!(exact.endpoint.queue_usage().0, 1);
        assert_eq!(sender.endpoint.queue_usage().0, 1);
    }

    #[test]
    fn recv_truncation_claims_before_copy_and_returns_wire_length() {
        let _context = packet_test_context();
        let net_ns = namespace();
        let receiver =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns).unwrap();
        submit(
            &sender,
            &raw_ipv4_frame(),
            Some(loopback_send_address(ProtocolSelector::from_host_order(
                0x0800,
            ))),
        )
        .unwrap();

        let mut bytes = [0_u8; 8];
        let mut dst = &mut bytes[..];
        let result = receiver
            .recv_with_nonblocking(&mut dst, ReceiveFlags::TRUNC, true)
            .unwrap();
        assert_eq!(result.copied_len(), bytes.len());
        assert_eq!(result.returned_len(), raw_ipv4_frame().len());
        assert!(result.message_truncated());
        assert_eq!(result.address().packet_type(), PacketType::OUTGOING);
        let ingress = receiver.endpoint.try_receive(false).unwrap();
        assert_eq!(ingress.metadata().packet_type, LinkPacketType::Host);
        assert_eq!(receiver.endpoint.queue_usage().0, 0);
    }

    #[test]
    fn ordinary_copy_fault_consumes_while_peek_fault_retains() {
        let _context = packet_test_context();
        let net_ns = namespace();
        let receiver =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns.clone())
                .unwrap();
        receiver
            .set_packet_option(SetPacketOption::IgnoreOutgoing(true))
            .unwrap();
        let sender =
            PacketSocket::try_new(PacketSocketType::Raw, ProtocolSelector::All, net_ns).unwrap();

        submit(
            &sender,
            &raw_ipv4_frame(),
            Some(loopback_send_address(ProtocolSelector::from_host_order(
                0x0800,
            ))),
        )
        .unwrap();
        assert_eq!(receiver.endpoint.queue_usage().0, 1);
        let mut ordinary = FaultDst {
            remaining: raw_ipv4_frame().len(),
        };
        assert_eq!(
            receiver.recv_with_nonblocking(&mut ordinary, ReceiveFlags::EMPTY, true),
            Err(AxError::BadAddress)
        );
        assert_eq!(receiver.endpoint.queue_usage().0, 0);

        submit(
            &sender,
            &raw_ipv4_frame(),
            Some(loopback_send_address(ProtocolSelector::from_host_order(
                0x0800,
            ))),
        )
        .unwrap();
        assert_eq!(receiver.endpoint.queue_usage().0, 1);
        let mut peek = FaultDst {
            remaining: raw_ipv4_frame().len(),
        };
        assert_eq!(
            receiver.recv_with_nonblocking(&mut peek, ReceiveFlags::PEEK, true),
            Err(AxError::BadAddress)
        );
        assert_eq!(receiver.endpoint.queue_usage().0, 1);
    }

    #[test]
    fn file_write_rejects_a_source_shorter_than_its_packet_length() {
        let _context = packet_test_context();
        let socket = PacketSocket::try_new(
            PacketSocketType::Raw,
            ProtocolSelector::Disabled,
            namespace(),
        )
        .unwrap();
        socket
            .bind(PacketBindRequest::new(
                InterfaceIndex::exact(1).unwrap(),
                ProtocolSelector::from_host_order(0x0800),
            ))
            .unwrap();
        let mut source = ShortSrc {
            bytes: &[1, 2, 3],
            offset: 0,
            advertised: 4,
        };
        assert_eq!(socket.write(&mut source), Err(AxError::UnexpectedEof));
    }
}
