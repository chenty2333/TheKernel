//! Namespace-global RFC4960 SCTP dispatcher.  Raw protocol-132 queues have a
//! single consumer; association sockets are selected here, never at raw I/O.
use crate::{
    NetStack, RecvFlags, RecvOptions, SendOptions, Shutdown, Socket, SocketAddrEx, SocketOps,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, SetSocketOption, SocketFault},
    raw::{RawSocket, RawSocketFamily},
};
use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use axerrno::{AxError, AxResult};
use axhal::time::TimeValue;
use axio::prelude::*;
use axpoll::{
    IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable, PreparedPollRegistration,
};
use axsync::Mutex;
use axtask::future::{TimerRegistrationError, sleep_until};
use core::{
    cmp,
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::{Context, Poll},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
pub const IPPROTO_SCTP: u8 = 132;

/// Per-record ULP parameters supplied by the SCTP ancillary ABI.  Keeping
/// this independent of a kernel cmsg type lets the transport remain usable by
/// every axnet consumer while the Linux adapter owns cmsghdr parsing.
#[derive(Clone, Copy, Debug, Default)]
pub struct SctpSendMetadata {
    pub stream: u16,
    pub flags: u16,
    pub ppid: u32,
    pub context: u32,
    pub pr_policy: u16,
    pub pr_value: u32,
}

/// Metadata of the record just delivered by `recvmsg`.  SCTP is message
/// oriented: this is retained with the queued record rather than reconstructed
/// from mutable association state after the record has been consumed.
#[derive(Clone, Copy, Debug, Default)]
pub struct SctpRecvMetadata {
    pub stream: u16,
    pub ssn: u16,
    pub flags: u16,
    pub ppid: u32,
    pub tsn: u32,
    pub cumtsn: u32,
    pub context: u32,
    pub assoc_id: i32,
    pub next: Option<SctpNextMetadata>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SctpNextMetadata {
    pub stream: u16,
    pub flags: u16,
    pub ppid: u32,
    pub length: u32,
    pub assoc_id: i32,
}
const H: usize = 12;
const DATA: u8 = 0;
const INIT: u8 = 1;
const INIT_ACK: u8 = 2;
const SACK: u8 = 3;
const HEARTBEAT: u8 = 4;
const HEARTBEAT_ACK: u8 = 5;
const ABORT: u8 = 6;
const SHUTDOWN: u8 = 7;
const SHUTDOWN_ACK: u8 = 8;
const COOKIE_ECHO: u8 = 10;
const COOKIE_ACK: u8 = 11;
const SHUTDOWN_COMPLETE: u8 = 14;
const FORWARD_TSN: u8 = 192;
const I_FORWARD_TSN: u8 = 194;
const ASCONF: u8 = 193;
const ASCONF_ACK: u8 = 128;
const I_DATA: u8 = 64;
const COOKIE: u16 = 7;
const SUPPORTED_EXTENSIONS: u16 = 0x8008;
const RTO: u64 = 1_000_000_000;
const LIFE: u64 = 60_000_000_000;
type HmacSha256 = Hmac<Sha256>;
#[derive(Copy, Clone, Eq, PartialEq)]
enum Phase {
    Idle,
    Listen,
    CookieWait,
    CookieEchoed,
    Established,
    ShutdownSent,
    Closed,
}
#[derive(Clone)]
struct Record {
    peer: SocketAddr,
    data: Vec<u8>,
    stream: u16,
    ssn: u16,
    tsn: u32,
    ppid: u32,
    context: u32,
    flags: u16,
}
#[derive(Clone)]
struct Sent {
    tsn: u32,
    wire: Vec<u8>,
    when: u64,
    retries: u8,
    created: u64,
    pr_policy: u16,
    pr_value: u32,
    stream: u16,
    ssn: u16,
    bytes: u32,
    misses: u8,
}
struct State {
    local: Option<SocketAddr>,
    peer: Option<SocketAddr>,
    local_paths: Vec<SocketAddr>,
    peer_paths: Vec<SocketAddr>,
    active_path: usize,
    phase: Phase,
    vtag: u32,
    peer_tag: u32,
    next_tsn: u32,
    cum: u32,
    ssn: u16,
    mid: u32,
    rwnd: u32,
    cwnd: u32,
    outstanding_bytes: u32,
    queued_bytes: u32,
    ssthresh: u32,
    rx: VecDeque<Record>,
    pending: VecDeque<Arc<Inner>>,
    out: VecDeque<Sent>,
    received: BTreeMap<u32, (u16, u16, u32, u32, u8, u32, Vec<u8>)>,
    frags: BTreeMap<(u16, u32), Vec<(bool, bool, u32, u32, u8, u32, Vec<u8>)>>,
    backlog: usize,
    t1: u64,
    t1_generation: u64,
    t1_retries: u16,
    t1_wire: Vec<u8>,
    t3: u64,
    init_out: u16,
    init_in: u16,
    init_attempts: u16,
    init_timeout: u16,
    nodelay: bool,
    autoclose: u32,
    rto_initial: u32,
    rto_max: u32,
    rto_min: u32,
    event_mask: [u8; 14],
    recv_rcvinfo: bool,
    recv_nxtinfo: bool,
    peer_pr_supported: bool,
    peer_idata_supported: bool,
    peer_asconf_supported: bool,
    asconf_serial: u32,
    asconf_pending: BTreeMap<u32, Vec<(bool, SocketAddr)>>,
}
impl State {
    fn new() -> Self {
        Self {
            local: None,
            peer: None,
            local_paths: Vec::new(),
            peer_paths: Vec::new(),
            active_path: 0,
            phase: Phase::Idle,
            vtag: 0,
            peer_tag: 0,
            next_tsn: 1,
            cum: 0,
            ssn: 0,
            mid: 0,
            rwnd: 262144,
            cwnd: 4800,
            outstanding_bytes: 0,
            queued_bytes: 0,
            ssthresh: 65536,
            rx: VecDeque::new(),
            pending: VecDeque::new(),
            out: VecDeque::new(),
            received: BTreeMap::new(),
            frags: BTreeMap::new(),
            backlog: 0,
            t1: 0,
            t1_generation: 0,
            t1_retries: 0,
            t1_wire: Vec::new(),
            t3: 0,
            init_out: 10,
            init_in: 10,
            init_attempts: 8,
            init_timeout: 60_000,
            nodelay: false,
            autoclose: 0,
            rto_initial: 1_000,
            rto_max: 60_000,
            rto_min: 1_000,
            event_mask: [0; 14],
            recv_rcvinfo: false,
            recv_nxtinfo: false,
            peer_pr_supported: false,
            peer_idata_supported: false,
            peer_asconf_supported: false,
            asconf_serial: 1,
            asconf_pending: BTreeMap::new(),
        }
    }
}
struct Inner {
    stack: Arc<NetStack>,
    family: RawSocketFamily,
    state: Mutex<State>,
    general: GeneralOptions,
    poll: PollSet,
    timer: Mutex<Option<SctpTimer>>,
    rd: AtomicBool,
    wr: AtomicBool,
}
struct SctpTimer {
    deadline: TimeValue,
    // `Inner` is retained through the namespace dispatcher and is therefore
    // shared by socket operations.  The timer registration must consequently
    // be movable between task contexts just like the network service timer.
    future: Pin<Box<dyn Future<Output = Result<(), TimerRegistrationError>> + Send>>,
}
struct Keys {
    now: [u8; 32],
    old: [u8; 32],
    at: u64,
}
pub struct SctpDispatcher {
    v4: RawSocket,
    v6: RawSocket,
    eps: Mutex<Vec<Weak<Inner>>>,
    rng: AtomicU64,
    keys: Mutex<Keys>,
}
impl SctpDispatcher {
    /// RFC 1982 serial-number ordering for the 32-bit TSN space.
    fn tsn_after(left: u32, right: u32) -> bool {
        left != right && (left.wrapping_sub(right) as i32) > 0
    }
    pub(crate) fn try_new(stack: Arc<NetStack>) -> AxResult<Arc<Self>> {
        let v4 = RawSocket::new(stack.clone(), RawSocketFamily::Ipv4, 132)?;
        let v6 = RawSocket::new(stack, RawSocketFamily::Ipv6, 132)?;
        v4.set_header_included(true);
        v6.set_header_included(true);
        let seed =
            axhal::time::wall_time_nanos() ^ ((&v4 as *const _ as usize) as u64).rotate_left(19);
        let mut a = [0; 32];
        let mut b = [0; 32];
        Self::seed(seed, &mut a);
        Self::seed(seed.rotate_left(31), &mut b);
        Arc::try_new(Self {
            v4,
            v6,
            eps: Mutex::new(Vec::new()),
            rng: AtomicU64::new(seed | 1),
            keys: Mutex::new(Keys {
                now: a,
                old: b,
                at: axhal::time::wall_time_nanos(),
            }),
        })
        .map_err(|_| AxError::NoMemory)
    }
    fn seed(mut x: u64, out: &mut [u8; 32]) {
        for v in out.chunks_exact_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.copy_from_slice(&x.to_be_bytes())
        }
    }
    fn random(&self) -> u32 {
        let mut x = self.rng.fetch_add(0x9e3779b97f4a7c15, Ordering::Relaxed)
            ^ axhal::time::wall_time_nanos().rotate_left(17);
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58476d1ce4e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d049bb133111eb);
        let n = (x ^ (x >> 31)) as u32;
        if n == 0 { 1 } else { n }
    }
    fn register(&self, x: &Arc<Inner>) -> AxResult {
        let mut e = self.eps.lock();
        e.retain(|w| w.strong_count() != 0);
        e.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        e.push(Arc::downgrade(x));
        Ok(())
    }
    fn rotate(&self) {
        let n = axhal::time::wall_time_nanos();
        let mut k = self.keys.lock();
        if n.saturating_sub(k.at) >= LIFE {
            k.old = k.now;
            Self::seed(self.rng.fetch_xor(n, Ordering::Relaxed), &mut k.now);
            k.at = n
        }
    }
    fn caddr(v: &mut Vec<u8>, a: IpAddr) {
        match a {
            IpAddr::V4(x) => {
                v.push(4);
                v.extend_from_slice(&x.octets())
            }
            IpAddr::V6(x) => {
                v.push(6);
                v.extend_from_slice(&x.octets())
            }
        }
    }
    fn cookie(
        &self,
        l: SocketAddr,
        p: SocketAddr,
        ct: u32,
        st: u32,
        tsn: u32,
        peer_capabilities: u8,
    ) -> Vec<u8> {
        self.rotate();
        let mut v = Vec::new();
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&axhal::time::wall_time_nanos().to_be_bytes());
        v.extend_from_slice(&l.port().to_be_bytes());
        v.extend_from_slice(&p.port().to_be_bytes());
        v.extend_from_slice(&ct.to_be_bytes());
        v.extend_from_slice(&st.to_be_bytes());
        v.extend_from_slice(&tsn.to_be_bytes());
        Self::caddr(&mut v, l.ip());
        Self::caddr(&mut v, p.ip());
        v.push(peer_capabilities);
        let key = self.keys.lock().now;
        let mut m = HmacSha256::new_from_slice(&key).unwrap();
        m.update(&v);
        v.extend_from_slice(&m.finalize().into_bytes());
        v
    }
    fn valid_cookie(&self, b: &[u8], l: SocketAddr, p: SocketAddr) -> Option<(u32, u32, u32, u8)> {
        if b.len() < 68 {
            return None;
        }
        let tag = b.len() - 32;
        let age = axhal::time::wall_time_nanos()
            .saturating_sub(u64::from_be_bytes(b.get(4..12)?.try_into().ok()?));
        if age > LIFE {
            return None;
        }
        let k = self.keys.lock();
        let ok = [&k.now, &k.old].iter().any(|key| {
            let mut m = HmacSha256::new_from_slice(*key).unwrap();
            m.update(&b[..tag]);
            m.verify_slice(&b[tag..]).is_ok()
        });
        drop(k);
        if !ok {
            return None;
        }
        let lp = u16::from_be_bytes(b.get(12..14)?.try_into().ok()?);
        let pp = u16::from_be_bytes(b.get(14..16)?.try_into().ok()?);
        if lp != l.port() || pp != p.port() {
            return None;
        }
        let ct = u32::from_be_bytes(b.get(16..20)?.try_into().ok()?);
        let st = u32::from_be_bytes(b.get(20..24)?.try_into().ok()?);
        let t = u32::from_be_bytes(b.get(24..28)?.try_into().ok()?);
        let mut pos = 28;
        let read = |b: &[u8], pos: &mut usize| -> Option<IpAddr> {
            let kind = *b.get(*pos)?;
            *pos += 1;
            match kind {
                4 => {
                    let x = IpAddr::V4(<[u8; 4]>::try_from(b.get(*pos..*pos + 4)?).ok()?.into());
                    *pos += 4;
                    Some(x)
                }
                6 => {
                    let x = IpAddr::V6(<[u8; 16]>::try_from(b.get(*pos..*pos + 16)?).ok()?.into());
                    *pos += 16;
                    Some(x)
                }
                _ => None,
            }
        };
        let a = read(b, &mut pos)?;
        let z = read(b, &mut pos)?;
        if pos.checked_add(1)? != tag {
            return None;
        }
        let peer_capabilities = *b.get(pos)?;
        (a == l.ip() && z == p.ip()).then_some((ct, st, t, peer_capabilities))
    }
    fn head(sp: u16, dp: u16, tag: u32) -> Vec<u8> {
        let mut v = vec![0; H];
        v[..2].copy_from_slice(&sp.to_be_bytes());
        v[2..4].copy_from_slice(&dp.to_be_bytes());
        v[4..8].copy_from_slice(&tag.to_be_bytes());
        v
    }
    /// RFC 4960's CRC32c covers the full SCTP packet with its checksum field
    /// zeroed.  The common-header checksum is little-endian on the wire.
    fn crc32c(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }
    fn wire_crc_valid(packet: &[u8]) -> bool {
        if packet.len() < H {
            return false;
        }
        let expected = u32::from_le_bytes(packet[8..12].try_into().unwrap());
        let mut packet = packet.to_vec();
        packet[8..12].fill(0);
        Self::crc32c(&packet) == expected
    }
    fn stamp_wire_crc(packet: &mut [u8]) {
        if packet.len() < H {
            return;
        }
        packet[8..12].fill(0);
        let checksum = Self::crc32c(packet);
        packet[8..12].copy_from_slice(&checksum.to_le_bytes());
    }
    fn ipv4_checksum(header: &[u8]) -> u16 {
        let mut sum = 0u32;
        for word in header.chunks_exact(2) {
            sum = sum.wrapping_add(u32::from(u16::from_be_bytes([word[0], word[1]])));
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !(sum as u16)
    }
    fn chunk(v: &mut Vec<u8>, ty: u8, fl: u8, b: &[u8]) -> AxResult {
        let n = 4usize.checked_add(b.len()).ok_or(AxError::OutOfRange)?;
        if n > u16::MAX as usize {
            return Err(AxError::OutOfRange);
        }
        v.push(ty);
        v.push(fl);
        v.extend_from_slice(&(n as u16).to_be_bytes());
        v.extend_from_slice(b);
        while v.len() % 4 != 0 {
            v.push(0)
        }
        Ok(())
    }
    fn emit_with_mode(
        &self,
        f: RawSocketFamily,
        l: SocketAddr,
        p: SocketAddr,
        l4: &[u8],
        nowait: bool,
    ) -> AxResult {
        let mut l4 = l4.to_vec();
        Self::stamp_wire_crc(&mut l4);
        let mut v = Vec::new();
        match (f, l.ip(), p.ip()) {
            (RawSocketFamily::Ipv4, IpAddr::V4(s), IpAddr::V4(d)) => {
                let n = 20 + l4.len();
                if n > 65535 {
                    return Err(AxError::OutOfRange);
                }
                v.resize(n, 0);
                v[0] = 0x45;
                v[2..4].copy_from_slice(&(n as u16).to_be_bytes());
                v[8] = 64;
                v[9] = 132;
                v[12..16].copy_from_slice(&s.octets());
                v[16..20].copy_from_slice(&d.octets());
                v[20..].copy_from_slice(&l4);
                let checksum = Self::ipv4_checksum(&v[..20]);
                v[10..12].copy_from_slice(&checksum.to_be_bytes());
                if nowait {
                    self.v4
                        .try_send_header_included_with_dont_route(&v, p, false)?;
                } else {
                    self.v4.send(
                        &v[..],
                        SendOptions {
                            to: Some(SocketAddrEx::Ip(p)),
                            ..Default::default()
                        },
                    )?;
                }
            }
            (RawSocketFamily::Ipv6, IpAddr::V6(s), IpAddr::V6(d)) => {
                if l4.len() > 65535 {
                    return Err(AxError::OutOfRange);
                }
                v.resize(40 + l4.len(), 0);
                v[0] = 0x60;
                v[4..6].copy_from_slice(&(l4.len() as u16).to_be_bytes());
                v[6] = 132;
                v[7] = 64;
                v[8..24].copy_from_slice(&s.octets());
                v[24..40].copy_from_slice(&d.octets());
                v[40..].copy_from_slice(&l4);
                if nowait {
                    self.v6
                        .try_send_header_included_with_dont_route(&v, p, false)?;
                } else {
                    self.v6.send(
                        &v[..],
                        SendOptions {
                            to: Some(SocketAddrEx::Ip(p)),
                            ..Default::default()
                        },
                    )?;
                }
            }
            _ => return Err(AxError::InvalidInput),
        };
        Ok(())
    }
    fn emit(&self, f: RawSocketFamily, l: SocketAddr, p: SocketAddr, l4: &[u8]) -> AxResult {
        self.emit_with_mode(f, l, p, l4, false)
    }
    fn emit_t1(&self, f: RawSocketFamily, l: SocketAddr, p: SocketAddr, l4: &[u8]) -> AxResult {
        self.emit_with_mode(f, l, p, l4, true)
    }
    fn route(&self, l: SocketAddr, p: SocketAddr, tag: u32, ty: u8) -> Option<Arc<Inner>> {
        for w in self.eps.lock().iter() {
            let Some(x) = w.upgrade() else { continue };
            let s = x.state.lock();
            if s.local.map(|a| a.port()) != Some(l.port()) {
                continue;
            }
            if !s.local_paths.is_empty() && !s.local_paths.contains(&l) {
                continue;
            }
            let matched = (matches!(s.phase, Phase::Established | Phase::ShutdownSent)
                && s.peer == Some(p)
                && s.vtag == tag)
                || (s.phase == Phase::CookieWait
                    && s.peer == Some(p)
                    && matches!(ty, INIT_ACK | ABORT)
                    && tag == s.vtag)
                || (s.phase == Phase::CookieEchoed
                    && s.peer == Some(p)
                    && matches!(ty, COOKIE_ACK | ABORT)
                    && tag == s.vtag)
                || (s.phase == Phase::Listen && ty == INIT && tag == 0)
                || (s.phase == Phase::Listen && ty == COOKIE_ECHO);
            drop(s);
            if matched {
                return Some(x);
            }
        }
        None
    }
    pub fn drain(&self) {
        for raw in [&self.v4, &self.v6] {
            for _ in 0..64 {
                let mut f = vec![0; 65535];
                let n = match raw.recv(
                    &mut f[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..Default::default()
                    },
                ) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                f.truncate(n);
                let h = match f.first().map(|x| x >> 4) {
                    Some(4) => usize::from(f[0] & 15) * 4,
                    Some(6) => 40,
                    _ => continue,
                };
                if h + H + 4 > f.len() {
                    continue;
                }
                let src = if h == 40 {
                    IpAddr::V6(<[u8; 16]>::try_from(&f[8..24]).unwrap().into())
                } else {
                    IpAddr::V4(<[u8; 4]>::try_from(&f[12..16]).unwrap().into())
                };
                let dst = if h == 40 {
                    IpAddr::V6(<[u8; 16]>::try_from(&f[24..40]).unwrap().into())
                } else {
                    IpAddr::V4(<[u8; 4]>::try_from(&f[16..20]).unwrap().into())
                };
                let q = &f[h..];
                if !Self::wire_crc_valid(q) {
                    continue;
                }
                let p = SocketAddr::new(src, u16::from_be_bytes([q[0], q[1]]));
                let l = SocketAddr::new(dst, u16::from_be_bytes([q[2], q[3]]));
                let tag = u32::from_be_bytes(q[4..8].try_into().unwrap());
                let mut o = H;
                while o + 4 <= q.len() {
                    let ty = q[o];
                    let fl = q[o + 1];
                    let len = usize::from(u16::from_be_bytes([q[o + 2], q[o + 3]]));
                    if len < 4 || o + len > q.len() {
                        break;
                    }
                    if let Some(x) = self.route(l, p, tag, ty) {
                        self.receive(&x, l, p, tag, ty, fl, &q[o + 4..o + len])
                    }
                    o = (o + len + 3) & !3
                }
            }
        }
    }
    fn receive(
        &self,
        x: &Arc<Inner>,
        l: SocketAddr,
        p: SocketAddr,
        tag: u32,
        ty: u8,
        fl: u8,
        b: &[u8],
    ) {
        match ty {
            INIT => self.init(x, l, p, b),
            INIT_ACK => self.init_ack(x, p, b),
            COOKIE_ECHO => self.echo(x, l, p, b),
            COOKIE_ACK => {
                let mut s = x.state.lock();
                let established = if s.phase == Phase::CookieEchoed {
                    s.phase = Phase::Established;
                    s.t1 = 0;
                    s.t1_generation = s.t1_generation.wrapping_add(1);
                    s.t1_wire.clear();
                    true
                } else {
                    false
                };
                drop(s);
                if established {
                    // Connection completion is a fresh successful
                    // association, so it supersedes an older deferred fault.
                    x.general.clear_pending_error();
                    x.poll.wake();
                }
            }
            DATA => self.data(x, p, tag, fl, b, None, 0),
            I_DATA => self.idata(x, p, tag, fl, b),
            SACK => self.sack(x, b),
            FORWARD_TSN => self.forward_tsn(x, p, tag, b),
            I_FORWARD_TSN => self.iforward_tsn(x, p, tag, b),
            ASCONF => self.asconf(x, p, tag, b),
            ASCONF_ACK => self.asconf_ack(x, p, tag, b),
            HEARTBEAT => {
                let _ = self.send(x, HEARTBEAT_ACK, 0, b, tag);
            }
            SHUTDOWN => {
                x.wr.store(true, Ordering::Release);
                let mut s = x.state.lock();
                s.phase = Phase::ShutdownSent;
                let t = s.peer_tag;
                drop(s);
                let _ = self.send(x, SHUTDOWN_ACK, 0, &[], t);
                x.poll.wake();
            }
            SHUTDOWN_ACK => {
                x.rd.store(true, Ordering::Release);
                x.wr.store(true, Ordering::Release);
                x.state.lock().phase = Phase::Closed;
                let _ = self.send(x, SHUTDOWN_COMPLETE, 0, &[], tag);
                x.poll.wake();
            }
            ABORT => {
                x.rd.store(true, Ordering::Release);
                x.wr.store(true, Ordering::Release);
                let mut s = x.state.lock();
                let fault = match s.phase {
                    Phase::CookieWait | Phase::CookieEchoed => SocketFault::ConnectionRefused,
                    Phase::Established | Phase::ShutdownSent => SocketFault::ConnectionReset,
                    _ => SocketFault::ConnectionReset,
                };
                s.phase = Phase::Closed;
                s.t1 = 0;
                s.t1_generation = s.t1_generation.wrapping_add(1);
                s.t1_wire.clear();
                drop(s);
                x.general.set_pending_error(fault);
                x.poll.wake();
            }
            _ => {}
        }
    }
    fn send(&self, x: &Inner, ty: u8, fl: u8, b: &[u8], tag: u32) -> AxResult {
        let s = x.state.lock();
        let (l, p) = (
            s.local.ok_or(AxError::InvalidInput)?,
            s.peer.ok_or(AxError::NotConnected)?,
        );
        let mut v = Self::head(l.port(), p.port(), tag);
        Self::chunk(&mut v, ty, fl, b)?;
        drop(s);
        self.emit(x.family, l, p, &v)
    }
    fn forward_tsn_supported(init: &[u8]) -> bool {
        let mut offset = 16usize;
        while offset + 4 <= init.len() {
            let kind = u16::from_be_bytes([init[offset], init[offset + 1]]);
            let length = usize::from(u16::from_be_bytes([init[offset + 2], init[offset + 3]]));
            if length < 4 || offset + length > init.len() {
                return false;
            }
            if kind == SUPPORTED_EXTENSIONS
                && init[offset + 4..offset + length].contains(&FORWARD_TSN)
            {
                return true;
            }
            offset = (offset + length + 3) & !3;
        }
        false
    }
    fn asconf_supported(init: &[u8]) -> bool {
        let mut offset = 16usize;
        while offset + 4 <= init.len() {
            let kind = u16::from_be_bytes([init[offset], init[offset + 1]]);
            let length = usize::from(u16::from_be_bytes([init[offset + 2], init[offset + 3]]));
            if length < 4 || offset + length > init.len() {
                return false;
            }
            if kind == SUPPORTED_EXTENSIONS && init[offset + 4..offset + length].contains(&ASCONF) {
                return true;
            }
            offset = (offset + length + 3) & !3;
        }
        false
    }
    fn append_forward_tsn_extension(payload: &mut Vec<u8>) {
        payload.extend_from_slice(&SUPPORTED_EXTENSIONS.to_be_bytes());
        payload.extend_from_slice(&7u16.to_be_bytes());
        payload.push(FORWARD_TSN);
        payload.push(ASCONF);
        payload.push(I_DATA);
        while payload.len() % 4 != 0 {
            payload.push(0);
        }
    }
    fn append_address_parameters(payload: &mut Vec<u8>, paths: &[SocketAddr]) {
        for path in paths {
            match path.ip() {
                IpAddr::V4(ip) => {
                    payload.extend_from_slice(&5u16.to_be_bytes());
                    payload.extend_from_slice(&8u16.to_be_bytes());
                    payload.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    payload.extend_from_slice(&6u16.to_be_bytes());
                    payload.extend_from_slice(&20u16.to_be_bytes());
                    payload.extend_from_slice(&ip.octets());
                }
            }
            while payload.len() % 4 != 0 {
                payload.push(0);
            }
        }
    }
    fn init(&self, x: &Arc<Inner>, l: SocketAddr, p: SocketAddr, b: &[u8]) {
        if b.len() < 16 || x.state.lock().phase != Phase::Listen {
            return;
        }
        let ct = u32::from_be_bytes(b[..4].try_into().unwrap());
        if ct == 0 {
            return;
        }
        let tsn = u32::from_be_bytes(b[12..16].try_into().unwrap());
        // INIT address parameters are candidates, never authority to switch
        // families. Reject malformed/mixed-family advertisements before a
        // cookie binds this association.
        let mut offset = 16usize;
        while offset + 4 <= b.len() {
            let kind = u16::from_be_bytes(b[offset..offset + 2].try_into().unwrap());
            let length = usize::from(u16::from_be_bytes(
                b[offset + 2..offset + 4].try_into().unwrap(),
            ));
            if length < 4 || offset + length > b.len() {
                return;
            }
            if (kind == 5 && (length != 8 || !p.is_ipv4()))
                || (kind == 6 && (length != 20 || p.is_ipv4()))
            {
                return;
            }
            offset = (offset + length + 3) & !3;
        }
        let st = self.random();
        let peer_pr_supported = Self::forward_tsn_supported(b);
        let peer_asconf_supported = Self::asconf_supported(b);
        let peer_capabilities =
            u8::from(peer_pr_supported) | (u8::from(peer_asconf_supported) << 1);
        let cookie = self.cookie(l, p, ct, st, tsn, peer_capabilities);
        let mut z = Vec::new();
        z.extend_from_slice(&st.to_be_bytes());
        z.extend_from_slice(&262144u32.to_be_bytes());
        z.extend_from_slice(&10u16.to_be_bytes());
        z.extend_from_slice(&10u16.to_be_bytes());
        z.extend_from_slice(&self.random().to_be_bytes());
        z.extend_from_slice(&COOKIE.to_be_bytes());
        z.extend_from_slice(&((4 + cookie.len()) as u16).to_be_bytes());
        z.extend_from_slice(&cookie);
        while z.len() % 4 != 0 {
            z.push(0)
        }
        Self::append_forward_tsn_extension(&mut z);
        let local_paths = x.state.lock().local_paths.clone();
        Self::append_address_parameters(&mut z, &local_paths);
        let mut v = Self::head(l.port(), p.port(), ct);
        let _ = Self::chunk(&mut v, INIT_ACK, 0, &z);
        let _ = self.emit(x.family, l, p, &v);
    }
    fn init_ack(&self, x: &Arc<Inner>, p: SocketAddr, b: &[u8]) {
        if b.len() < 20 {
            return;
        }
        let st = u32::from_be_bytes(b[..4].try_into().unwrap());
        let mut o = 16;
        let mut c = None;
        let mut peer_pr_supported = false;
        let mut peer_asconf_supported = false;
        let mut peer_idata_supported = false;
        let mut advertised_paths = Vec::new();
        while o + 4 <= b.len() {
            let t = u16::from_be_bytes(b[o..o + 2].try_into().unwrap());
            let n = usize::from(u16::from_be_bytes(b[o + 2..o + 4].try_into().unwrap()));
            if n < 4 || o + n > b.len() {
                return;
            }
            if t == COOKIE {
                c = Some(b[o + 4..o + n].to_vec());
            } else if t == SUPPORTED_EXTENSIONS && b[o + 4..o + n].contains(&FORWARD_TSN) {
                peer_pr_supported = true;
                peer_asconf_supported = b[o + 4..o + n].contains(&ASCONF);
                peer_idata_supported = b[o + 4..o + n].contains(&I_DATA);
            } else if t == 5 && n == 8 {
                advertised_paths.push(SocketAddr::new(
                    IpAddr::V4(core::net::Ipv4Addr::new(
                        b[o + 4],
                        b[o + 5],
                        b[o + 6],
                        b[o + 7],
                    )),
                    p.port(),
                ));
            } else if t == 6 && n == 20 {
                advertised_paths.push(SocketAddr::new(
                    IpAddr::V6(<[u8; 16]>::try_from(&b[o + 4..o + 20]).unwrap().into()),
                    p.port(),
                ));
            }
            o = (o + n + 3) & !3
        }
        let Some(c) = c else { return };
        let mut s = x.state.lock();
        if s.phase != Phase::CookieWait || s.peer != Some(p) || st == 0 {
            return;
        }
        s.peer_tag = st;
        s.peer_pr_supported = peer_pr_supported;
        s.peer_asconf_supported = peer_asconf_supported;
        s.peer_idata_supported = peer_idata_supported;
        for path in advertised_paths {
            if path.is_ipv4() == p.is_ipv4() && !s.peer_paths.contains(&path) {
                s.peer_paths.push(path);
            }
        }
        s.phase = Phase::CookieEchoed;
        s.t1 = axhal::time::wall_time_nanos();
        s.t1_generation = s.t1_generation.wrapping_add(1);
        s.t1_retries = 0;
        let local = s.local.expect("cookie echo has local endpoint");
        let mut wire = Self::head(local.port(), p.port(), st);
        if Self::chunk(&mut wire, COOKIE_ECHO, 0, &c).is_err() {
            return;
        }
        s.t1_wire = wire.clone();
        drop(s);
        let _ = self.emit(x.family, local, p, &wire);
    }
    fn echo(&self, x: &Arc<Inner>, l: SocketAddr, p: SocketAddr, b: &[u8]) {
        let Some((ct, st, tsn, peer_capabilities)) = self.valid_cookie(b, l, p) else {
            return;
        };
        let mut s = x.state.lock();
        if s.phase != Phase::Listen || s.pending.len() >= s.backlog {
            return;
        }
        let a = match Arc::try_new(Inner {
            stack: x.stack.clone(),
            family: x.family,
            state: Mutex::new(State {
                local: Some(l),
                peer: Some(p),
                phase: Phase::Established,
                vtag: st,
                peer_tag: ct,
                next_tsn: self.random(),
                cum: tsn.wrapping_sub(1),
                peer_pr_supported: peer_capabilities & 1 != 0,
                peer_asconf_supported: peer_capabilities & 2 != 0,
                ..State::new()
            }),
            general: GeneralOptions::new(),
            poll: PollSet::new(),
            timer: Mutex::new(None),
            rd: AtomicBool::new(false),
            wr: AtomicBool::new(false),
        }) {
            Ok(a) => a,
            Err(_) => return,
        };
        // A completed passive association must become a dispatcher endpoint
        // before it is exposed in the accept queue; otherwise raw ingress can
        // race accept and be consumed by no association at all.
        if self.register(&a).is_err() {
            return;
        }
        s.pending.push_back(a);
        drop(s);
        let mut wire = Self::head(l.port(), p.port(), ct);
        let _ = Self::chunk(&mut wire, COOKIE_ACK, 0, &[]);
        let _ = self.emit(x.family, l, p, &wire);
        x.poll.wake();
    }
    fn data(
        &self,
        x: &Arc<Inner>,
        p: SocketAddr,
        tag: u32,
        fl: u8,
        b: &[u8],
        mid: Option<u32>,
        fsn: u32,
    ) {
        if b.len() < 12 {
            return;
        }
        let tsn = u32::from_be_bytes(b[..4].try_into().unwrap());
        let stream = u16::from_be_bytes(b[4..6].try_into().unwrap());
        let ssn = u16::from_be_bytes(b[6..8].try_into().unwrap());
        let mid = mid.unwrap_or(u32::from(ssn));
        let ppid = u32::from_be_bytes(b[8..12].try_into().unwrap());
        let mut s = x.state.lock();
        if s.phase != Phase::Established
            || s.peer != Some(p)
            || s.vtag != tag
            || !Self::tsn_after(tsn, s.cum)
        {
            return;
        }
        if s.received.contains_key(&tsn) {
            return;
        }
        s.received
            .insert(tsn, (stream, ssn, mid, fsn, fl, ppid, b[12..].to_vec()));
        // Cumulative ACK advances only over a gap-free TSN prefix.  Future
        // TSNs remain in `received`, and are reported by SACK gap blocks in a
        // fuller implementation rather than being mistaken for delivered.
        loop {
            let next_tsn = s.cum.wrapping_add(1);
            let Some((stream, ssn, mid, fsn, flags, ppid, fragment)) = s.received.remove(&next_tsn)
            else {
                break;
            };
            s.cum = next_tsn;
            let cumulative_tsn = s.cum;
            let fragments = s.frags.entry((stream, mid)).or_default();
            fragments.push((
                flags & 2 != 0,
                flags & 1 != 0,
                fsn,
                cumulative_tsn,
                flags,
                ppid,
                fragment,
            ));
            fragments.sort_unstable_by_key(|fragment| fragment.2);
            if fragments.first().is_some_and(|fragment| fragment.0)
                && fragments.last().is_some_and(|fragment| fragment.1)
            {
                let first_tsn = fragments[0].3;
                let flags = fragments.last().unwrap().4;
                let ppid = fragments[0].5;
                let mut data = Vec::new();
                for (_, _, _, _, _, _, fragment) in fragments.iter() {
                    data.extend_from_slice(fragment);
                }
                if data.len() <= 65536
                    && s.queued_bytes.saturating_add(data.len() as u32) <= 262_144
                {
                    s.queued_bytes = s.queued_bytes.saturating_add(data.len() as u32);
                    s.rx.push_back(Record {
                        peer: p,
                        data,
                        stream,
                        ssn,
                        tsn: first_tsn,
                        ppid,
                        context: 0,
                        flags: flags as u16,
                    });
                }
                s.frags.remove(&(stream, mid));
            }
        }
        let mut a = Vec::new();
        a.extend_from_slice(&s.cum.to_be_bytes());
        a.extend_from_slice(&s.rwnd.to_be_bytes());
        let mut gaps = Vec::new();
        let mut run = None::<(u16, u16)>;
        for tsn in s.received.keys().copied() {
            let delta = tsn.wrapping_sub(s.cum);
            if delta == 0 || delta > u32::from(u16::MAX) {
                continue;
            }
            let delta = delta as u16;
            match run {
                Some((start, end)) if delta == end.saturating_add(1) => run = Some((start, delta)),
                Some(range) => {
                    gaps.push(range);
                    run = Some((delta, delta));
                }
                None => run = Some((delta, delta)),
            }
        }
        if let Some(range) = run {
            gaps.push(range);
        }
        a.extend_from_slice(&(gaps.len() as u16).to_be_bytes());
        a.extend_from_slice(&0u16.to_be_bytes());
        for (start, end) in gaps {
            a.extend_from_slice(&start.to_be_bytes());
            a.extend_from_slice(&end.to_be_bytes());
        }
        let t = s.peer_tag;
        drop(s);
        let _ = self.send(x, SACK, 0, &a, t);
        x.poll.wake();
    }
    /// I-DATA keeps a 32-bit MID in its header.  The compact SCTP record ABI
    /// currently exposes the legacy 16-bit SSN, so retain its low word there
    /// while preserving full TSN-gap ordering in the common receive queue.
    /// B-bit fragments carry PPID; non-first fragments carry FSN instead and
    /// must not overwrite the message PPID.
    fn idata(&self, x: &Arc<Inner>, p: SocketAddr, tag: u32, flags: u8, body: &[u8]) {
        if body.len() < 16 {
            return;
        }
        let mid = u32::from_be_bytes(body[8..12].try_into().unwrap());
        let fsn = if flags & 2 != 0 {
            0
        } else {
            u32::from_be_bytes(body[12..16].try_into().unwrap())
        };
        let mut data = Vec::new();
        data.extend_from_slice(&body[..4]);
        data.extend_from_slice(&body[6..8]);
        data.extend_from_slice(&(mid as u16).to_be_bytes());
        if flags & 2 != 0 {
            data.extend_from_slice(&body[12..16]);
        } else {
            data.extend_from_slice(&0u32.to_be_bytes());
        }
        data.extend_from_slice(&body[16..]);
        self.data(x, p, tag, flags, &data, Some(mid), fsn);
    }
    fn sack(&self, x: &Arc<Inner>, b: &[u8]) {
        if b.len() < 12 {
            return;
        }
        let c = u32::from_be_bytes(b[..4].try_into().unwrap());
        let r = u32::from_be_bytes(b[4..8].try_into().unwrap());
        let mut s = x.state.lock();
        s.rwnd = r;
        while s
            .out
            .front()
            .is_some_and(|v| v.tsn == c || Self::tsn_after(c, v.tsn))
        {
            let sent = s.out.pop_front().expect("front checked");
            s.outstanding_bytes = s.outstanding_bytes.saturating_sub(sent.bytes);
            if s.cwnd < s.ssthresh {
                s.cwnd = s.cwnd.saturating_add(1200)
            } else {
                s.cwnd = s.cwnd.saturating_add(1200 * 1200 / cmp::max(s.cwnd, 1))
            }
        }
        let gap_count = usize::from(u16::from_be_bytes(b[8..10].try_into().unwrap()));
        let mut fast_retransmit = Vec::new();
        if b.len() >= 12 + gap_count.saturating_mul(4) {
            for sent in s.out.iter_mut() {
                let acknowledged = b[12..12 + gap_count * 4].chunks_exact(4).any(|gap| {
                    let start = u32::from(u16::from_be_bytes(gap[..2].try_into().unwrap()));
                    let end = u32::from(u16::from_be_bytes(gap[2..4].try_into().unwrap()));
                    let delta = sent.tsn.wrapping_sub(c);
                    delta >= start && delta <= end
                });
                if acknowledged {
                    sent.misses = 0;
                } else {
                    sent.misses = sent.misses.saturating_add(1);
                    if sent.misses == 3 {
                        sent.when = axhal::time::wall_time_nanos();
                        sent.retries = sent.retries.saturating_add(1);
                        fast_retransmit.push(sent.wire.clone());
                    }
                }
            }
        }
        let route = s.local.zip(s.peer);
        x.poll.wake();
        drop(s);
        if let Some((local, peer)) = route {
            for wire in fast_retransmit {
                let _ = self.emit(x.family, local, peer, &wire);
            }
        }
    }
    fn forward_tsn(&self, x: &Arc<Inner>, p: SocketAddr, tag: u32, b: &[u8]) {
        if b.len() < 4 || (b.len() - 4) % 4 != 0 {
            return;
        }
        let new_cumulative = u32::from_be_bytes(b[..4].try_into().unwrap());
        let mut s = x.state.lock();
        if s.phase != Phase::Established
            || !s.peer_pr_supported
            || s.peer != Some(p)
            || s.vtag != tag
            || !Self::tsn_after(new_cumulative, s.cum)
        {
            return;
        }
        // A peer that negotiated PR-SCTP may skip undeliverable ordered
        // records.  Drop only incomplete reassembly slots; already queued
        // records preserve their delivery order and metadata.
        s.cum = new_cumulative;
        s.received.retain(|tsn, _| *tsn > new_cumulative);
        s.frags.clear();
        let mut sack = Vec::new();
        sack.extend_from_slice(&s.cum.to_be_bytes());
        sack.extend_from_slice(&s.rwnd.to_be_bytes());
        sack.extend_from_slice(&0u16.to_be_bytes());
        sack.extend_from_slice(&0u16.to_be_bytes());
        let peer_tag = s.peer_tag;
        drop(s);
        let _ = self.send(x, SACK, 0, &sack, peer_tag);
        x.poll.wake();
    }
    fn iforward_tsn(&self, x: &Arc<Inner>, p: SocketAddr, tag: u32, b: &[u8]) {
        if b.len() < 4 || (b.len() - 4) % 8 != 0 {
            return;
        }
        let new_cumulative = u32::from_be_bytes(b[..4].try_into().unwrap());
        let mut skipped = Vec::new();
        for entry in b[4..].chunks_exact(8) {
            skipped.push((
                u16::from_be_bytes(entry[..2].try_into().unwrap()),
                u32::from_be_bytes(entry[4..8].try_into().unwrap()),
            ));
        }
        let mut s = x.state.lock();
        if s.phase != Phase::Established
            || !s.peer_pr_supported
            || s.peer != Some(p)
            || s.vtag != tag
            || !Self::tsn_after(new_cumulative, s.cum)
        {
            return;
        }
        s.cum = new_cumulative;
        s.received.retain(|tsn, _| *tsn > new_cumulative);
        s.frags.retain(|(stream, mid), _| {
            !skipped
                .iter()
                .any(|(skip_stream, skip_mid)| stream == skip_stream && mid <= skip_mid)
        });
        let mut sack = Vec::new();
        sack.extend_from_slice(&s.cum.to_be_bytes());
        sack.extend_from_slice(&s.rwnd.to_be_bytes());
        sack.extend_from_slice(&0u16.to_be_bytes());
        sack.extend_from_slice(&0u16.to_be_bytes());
        let peer_tag = s.peer_tag;
        drop(s);
        let _ = self.send(x, SACK, 0, &sack, peer_tag);
        x.poll.wake();
    }
    fn asconf(&self, x: &Arc<Inner>, p: SocketAddr, tag: u32, b: &[u8]) {
        if b.len() < 4 {
            return;
        }
        let serial = u32::from_be_bytes(b[..4].try_into().unwrap());
        let mut offset = 4usize;
        let mut s = x.state.lock();
        if s.phase != Phase::Established
            || !s.peer_asconf_supported
            || s.peer != Some(p)
            || s.vtag != tag
        {
            return;
        }
        while offset + 8 <= b.len() {
            let kind = u16::from_be_bytes(b[offset..offset + 2].try_into().unwrap());
            let len = usize::from(u16::from_be_bytes(
                b[offset + 2..offset + 4].try_into().unwrap(),
            ));
            if len < 8 || offset + len > b.len() {
                return;
            }
            let address_param = &b[offset + 8..offset + len];
            let address = match (
                address_param
                    .get(..2)
                    .and_then(|kind| <[u8; 2]>::try_from(kind).ok())
                    .map(u16::from_be_bytes),
                address_param.len(),
            ) {
                (Some(5), length) if length >= 8 => Some(SocketAddr::new(
                    IpAddr::V4(core::net::Ipv4Addr::new(
                        address_param[4],
                        address_param[5],
                        address_param[6],
                        address_param[7],
                    )),
                    p.port(),
                )),
                (Some(6), length) if length >= 20 => Some(SocketAddr::new(
                    IpAddr::V6(<[u8; 16]>::try_from(&address_param[4..20]).unwrap().into()),
                    p.port(),
                )),
                _ => None,
            };
            if let Some(address) = address {
                // An association never accepts an ASCONF path from the other
                // IP family: that would evade the family-specific raw route.
                if address.is_ipv4() != p.is_ipv4() {
                    return;
                }
                if kind == 0xc001 && !s.peer_paths.contains(&address) {
                    s.peer_paths.push(address);
                }
                if kind == 0xc002 {
                    s.peer_paths.retain(|candidate| *candidate != address);
                }
            }
            offset = (offset + len + 3) & !3;
        }
        let peer_tag = s.peer_tag;
        drop(s);
        let _ = self.send(x, ASCONF_ACK, 0, &serial.to_be_bytes(), peer_tag);
    }
    fn asconf_ack(&self, x: &Arc<Inner>, p: SocketAddr, tag: u32, b: &[u8]) {
        if b.len() != 4 {
            return;
        }
        let serial = u32::from_be_bytes(b.try_into().unwrap());
        let mut s = x.state.lock();
        if s.phase == Phase::Established && s.peer == Some(p) && s.vtag == tag {
            s.asconf_pending.remove(&serial);
        }
    }
    fn timers(&self, x: &Inner) {
        let n = axhal::time::wall_time_nanos();
        let mut send = Vec::new();
        let mut forward = None;
        let mut t1_timed_out = false;
        {
            let mut s = x.state.lock();
            if matches!(s.phase, Phase::CookieWait | Phase::CookieEchoed) && s.t1 != 0 {
                let base = u64::from(s.init_timeout.max(1)).saturating_mul(1_000_000);
                let shift = u32::from(s.t1_retries).min(16);
                let interval = base
                    .saturating_mul(1u64 << shift)
                    .min(u64::from(s.rto_max).saturating_mul(1_000_000));
                if n.saturating_sub(s.t1) >= interval {
                    if s.t1_retries >= s.init_attempts {
                        s.phase = Phase::Closed;
                        s.t1 = 0;
                        s.t1_generation = s.t1_generation.wrapping_add(1);
                        s.t1_wire.clear();
                        t1_timed_out = true;
                    } else if let (Some(local), Some(peer)) = (s.local, s.peer) {
                        let wire = s.t1_wire.clone();
                        if !wire.is_empty() {
                            // T1 cancellation and submission are serialized
                            // by `state`: no late timer can put INIT or
                            // COOKIE_ECHO on the wire after ABORT, COOKIE_ACK,
                            // or write-side shutdown invalidates its generation.
                            // `emit_t1` is a non-polling, nonblocking raw
                            // submission path, so this does not retain the
                            // association mutex across ingress or TX waiting.
                            if self.emit_t1(x.family, local, peer, &wire).is_ok() {
                                // Count only a retry actually admitted to
                                // the raw TX queue.  Local route/TX pressure
                                // must not exhaust the peer retry budget.
                                s.t1_retries = s.t1_retries.saturating_add(1);
                            }
                            // Do not spin on local admission failure; retry
                            // at the same (unbacked-off) T1 interval.
                            s.t1 = n;
                        } else {
                            // A malformed/incomplete local handshake image is
                            // not a transmitted retry either; avoid a hot
                            // timer loop while the association is torn down.
                            s.t1 = n;
                        }
                    }
                }
            }
            if n.saturating_sub(s.t3) >= RTO {
                let peer_pr_supported = s.peer_pr_supported;
                let mut abandoned = Vec::new();
                for v in s.out.iter_mut() {
                    if n.saturating_sub(v.when) >= RTO {
                        let policy = v.pr_policy & 0x0030;
                        let ttl_expired = peer_pr_supported
                            && policy == 0x0010
                            && n.saturating_sub(v.created)
                                >= u64::from(v.pr_value).saturating_mul(1_000_000);
                        let rtx_exhausted = peer_pr_supported
                            && policy == 0x0020
                            && u32::from(v.retries) >= v.pr_value;
                        if ttl_expired || rtx_exhausted {
                            abandoned.push((v.tsn, v.stream, v.ssn));
                        } else {
                            v.when = n;
                            v.retries = v.retries.saturating_add(1);
                            // RFC 3758 PRIO does not abandon records; it
                            // changes their scheduler order.  Lower values
                            // are sent first for both retransmit and failover.
                            let priority = if policy == 0x0030 {
                                v.pr_value
                            } else {
                                u32::MAX
                            };
                            send.push((priority, v.wire.clone()))
                        }
                    }
                }
                if !abandoned.is_empty() {
                    let highest = abandoned
                        .iter()
                        .map(|entry| entry.0)
                        .max()
                        .expect("nonempty");
                    s.out.retain(|entry| {
                        !abandoned.iter().any(|abandoned| abandoned.0 == entry.tsn)
                    });
                    if peer_pr_supported {
                        let mut body = Vec::new();
                        body.extend_from_slice(&highest.to_be_bytes());
                        for (_, stream, ssn) in abandoned {
                            body.extend_from_slice(&stream.to_be_bytes());
                            body.extend_from_slice(&0u16.to_be_bytes());
                            body.extend_from_slice(&u32::from(ssn).to_be_bytes());
                        }
                        forward = Some((body, s.peer_tag, s.peer_idata_supported));
                    }
                }
                if !send.is_empty() {
                    s.ssthresh = cmp::max(s.cwnd / 2, 2400);
                    s.cwnd = 1200;
                    // A retransmission timeout marks the current primary
                    // suspect.  Move to the next CONNECTX path before
                    // replaying the exact DATA wire image; a later SACK
                    // naturally confirms that path as active.
                    if !s.peer_paths.is_empty() {
                        s.active_path = (s.active_path + 1) % s.peer_paths.len();
                        s.peer = Some(s.peer_paths[s.active_path]);
                    }
                }
            }
        }
        if t1_timed_out {
            x.rd.store(true, Ordering::Release);
            x.wr.store(true, Ordering::Release);
            x.general.set_pending_error(SocketFault::TimedOut);
            x.poll.wake();
        }
        send.sort_unstable_by_key(|(priority, _)| *priority);
        for (_, v) in send {
            let s = x.state.lock();
            if let (Some(default_local), Some(p)) = (s.local, s.peer) {
                let l = s
                    .local_paths
                    .iter()
                    .copied()
                    .find(|candidate| candidate.is_ipv4() == p.is_ipv4())
                    .unwrap_or(default_local);
                drop(s);
                let _ = self.emit(x.family, l, p, &v);
            }
        }
        if let Some((body, tag, idata)) = forward {
            let _ = self.send(
                x,
                if idata { I_FORWARD_TSN } else { FORWARD_TSN },
                0,
                &body,
                tag,
            );
        }
    }
}
pub struct SctpSocket {
    inner: Arc<Inner>,
    dispatcher: Arc<SctpDispatcher>,
}
/// Holds the listener queue head without consuming it.  Dropping the token is
/// deliberately a no-op: policy rejection leaves the association pending.
pub struct SctpAcceptReservation<'a> {
    listener: &'a SctpSocket,
    accepted: Arc<Inner>,
}
impl SctpAcceptReservation<'_> {
    pub fn identity(&self) -> SocketAddr {
        self.accepted
            .state
            .lock()
            .peer
            .expect("completed SCTP association")
    }
    pub fn commit(self) -> AxResult<Socket> {
        let mut s = self.listener.inner.state.lock();
        if !s
            .pending
            .front()
            .is_some_and(|head| Arc::ptr_eq(head, &self.accepted))
        {
            return Err(AxError::WouldBlock);
        }
        let inner = s.pending.pop_front().expect("head verified");
        Ok(Socket::Sctp(SctpSocket {
            inner,
            dispatcher: self.listener.dispatcher.clone(),
        }))
    }
}
impl SctpSocket {
    /// Stores a one-shot SCTP transport fault and publishes ERROR readiness.
    /// `general` and `poll` are deliberately used without `inner.state`
    /// held: deferred errors may be recorded by dispatcher paths that also
    /// inspect association state.
    pub(crate) fn set_pending_error(&self, error: SocketFault) {
        self.inner.general.set_pending_error(error);
        self.inner.poll.wake();
    }

    pub(crate) fn retry_transfer<T>(
        &self,
        direction: crate::SocketTransferDirection,
        effective_nonblocking: bool,
        attempt: &mut impl FnMut() -> AxResult<T>,
    ) -> AxResult<T> {
        self.inner
            .general
            .transfer_poller(self, direction, effective_nonblocking, attempt)
    }

    fn next_t1_deadline(&self) -> u64 {
        let state = self.inner.state.lock();
        if !matches!(state.phase, Phase::CookieWait | Phase::CookieEchoed) || state.t1 == 0 {
            return 0;
        }
        let base = u64::from(state.init_timeout.max(1)).saturating_mul(1_000_000);
        let interval = base
            .saturating_mul(1u64 << u32::from(state.t1_retries).min(16))
            .min(u64::from(state.rto_max).saturating_mul(1_000_000));
        state.t1.saturating_add(interval)
    }

    fn arm_t1_timer(&self, context: &mut Context<'_>) -> Result<(), PollRegistrationError> {
        let deadline_nanos = self.next_t1_deadline();
        let mut timer = self.inner.timer.lock();
        if deadline_nanos == 0 {
            *timer = None;
            return Ok(());
        }
        let deadline = TimeValue::from_micros((deadline_nanos / 1_000) as _);
        if timer
            .as_ref()
            .is_none_or(|timer| timer.deadline != deadline)
        {
            let future =
                Box::try_new(sleep_until(deadline)).map_err(|_| PollRegistrationError::NoMemory)?;
            *timer = Some(SctpTimer {
                deadline,
                future: Box::into_pin(future),
            });
        }
        if let Some(active_timer) = timer.as_mut() {
            match active_timer.future.as_mut().poll(context) {
                Poll::Ready(Ok(())) => {
                    *timer = None;
                    context.waker().wake_by_ref();
                }
                Poll::Ready(Err(_)) => {
                    *timer = None;
                    return Err(PollRegistrationError::InvalidState);
                }
                Poll::Pending => {}
            }
        }
        Ok(())
    }

    fn send_asconf_change(&self, add: bool, address: SocketAddr) -> AxResult {
        let (serial, tag) = {
            let mut state = self.inner.state.lock();
            if state.phase != Phase::Established || !state.peer_asconf_supported {
                return Ok(());
            }
            let serial = state.asconf_serial;
            state.asconf_serial = state.asconf_serial.wrapping_add(1);
            state.asconf_pending.insert(serial, vec![(add, address)]);
            (serial, state.peer_tag)
        };
        let mut body = Vec::new();
        body.extend_from_slice(&serial.to_be_bytes());
        body.extend_from_slice(&(if add { 0xc001u16 } else { 0xc002u16 }).to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes());
        match address.ip() {
            IpAddr::V4(ip) => {
                body.splice(6..6, 16u16.to_be_bytes());
                body.extend_from_slice(&5u16.to_be_bytes());
                body.extend_from_slice(&8u16.to_be_bytes());
                body.extend_from_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                body.splice(6..6, 28u16.to_be_bytes());
                body.extend_from_slice(&6u16.to_be_bytes());
                body.extend_from_slice(&20u16.to_be_bytes());
                body.extend_from_slice(&ip.octets());
            }
        }
        self.dispatcher.send(&self.inner, ASCONF, 0, &body, tag)
    }
    pub fn initmsg(&self) -> [u16; 4] {
        let s = self.inner.state.lock();
        [s.init_out, s.init_in, s.init_attempts, s.init_timeout]
    }
    pub fn nodelay(&self) -> u32 {
        u32::from(self.inner.state.lock().nodelay)
    }
    pub fn autoclose(&self) -> u32 {
        self.inner.state.lock().autoclose
    }
    pub fn rtoinfo(&self) -> [u32; 4] {
        let s = self.inner.state.lock();
        [0, s.rto_initial, s.rto_max, s.rto_min]
    }
    pub fn events(&self) -> [u8; 14] {
        self.inner.state.lock().event_mask
    }
    pub fn recv_rcvinfo(&self) -> bool {
        self.inner.state.lock().recv_rcvinfo
    }
    pub fn recv_nxtinfo(&self) -> bool {
        self.inner.state.lock().recv_nxtinfo
    }
    pub fn set_recv_rcvinfo(&self, enabled: bool) {
        self.inner.state.lock().recv_rcvinfo = enabled;
    }
    pub fn set_recv_nxtinfo(&self, enabled: bool) {
        self.inner.state.lock().recv_nxtinfo = enabled;
    }
    pub fn set_initmsg(&self, out: u16, input: u16, attempts: u16, timeout: u16) {
        let mut s = self.inner.state.lock();
        s.init_out = out;
        s.init_in = input;
        s.init_attempts = attempts;
        s.init_timeout = timeout;
    }
    pub fn set_nodelay(&self, enabled: bool) {
        self.inner.state.lock().nodelay = enabled;
    }
    pub fn set_autoclose(&self, value: u32) {
        self.inner.state.lock().autoclose = value;
    }
    pub fn set_rtoinfo(&self, initial: u32, maximum: u32, minimum: u32) -> AxResult {
        if minimum == 0 || initial < minimum || maximum < initial {
            return Err(AxError::InvalidInput);
        }
        let mut s = self.inner.state.lock();
        s.rto_initial = initial;
        s.rto_max = maximum;
        s.rto_min = minimum;
        Ok(())
    }
    pub fn set_event(&self, event: usize, enabled: bool) -> AxResult {
        let mut s = self.inner.state.lock();
        let slot = s.event_mask.get_mut(event).ok_or(AxError::InvalidInput)?;
        *slot = u8::from(enabled);
        Ok(())
    }
    pub fn new(stack: Arc<NetStack>, family: RawSocketFamily) -> AxResult<Self> {
        let d = stack.sctp_dispatcher()?;
        let i = Arc::try_new(Inner {
            stack,
            family,
            state: Mutex::new(State::new()),
            general: GeneralOptions::new(),
            poll: PollSet::new(),
            timer: Mutex::new(None),
            rd: AtomicBool::new(false),
            wr: AtomicBool::new(false),
        })
        .map_err(|_| AxError::NoMemory)?;
        d.register(&i)?;
        Ok(Self {
            inner: i,
            dispatcher: d,
        })
    }
    pub(crate) fn recv_pending_len(&self) -> AxResult<usize> {
        self.dispatcher.drain();
        Ok(self
            .inner
            .state
            .lock()
            .rx
            .front()
            .map_or(0, |r| r.data.len()))
    }
    fn addr(a: SocketAddrEx) -> AxResult<SocketAddr> {
        a.into_ip().map_err(|_| AxError::InvalidInput)
    }
    fn conflict(&self, a: SocketAddr) -> bool {
        self.dispatcher
            .eps
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .any(|x| {
                !Arc::ptr_eq(&x, &self.inner)
                    && x.state.lock().local.is_some_and(|b| {
                        b.port() == a.port()
                            && (b.ip().is_unspecified()
                                || a.ip().is_unspecified()
                                || b.ip() == a.ip())
                    })
            })
    }
}
impl SctpSocket {
    pub fn prepare_accept(&self) -> AxResult<SctpAcceptReservation<'_>> {
        self.dispatcher.drain();
        let s = self.inner.state.lock();
        if s.phase != Phase::Listen {
            return Err(AxError::InvalidInput);
        }
        Ok(SctpAcceptReservation {
            listener: self,
            accepted: s.pending.front().cloned().ok_or(AxError::WouldBlock)?,
        })
    }
}
impl SctpSocket {
    /// SCTP_SOCKOPT_BINDX_ADD / DEL backend: every address belongs to the same
    /// association endpoint and is checked against namespace port ownership.
    pub fn bindx(&self, addresses: &[SocketAddr], add: bool) -> AxResult {
        if addresses.is_empty() {
            return Err(AxError::InvalidInput);
        }
        if !add {
            let mut state = self.inner.state.lock();
            for address in addresses {
                if state.local == Some(*address) {
                    return Err(AxError::InvalidInput);
                }
                state.local_paths.retain(|path| path != address)
            }
            drop(state);
            for address in addresses {
                self.send_asconf_change(false, *address)?;
            }
            return Ok(());
        }
        for address in addresses {
            if address.port() != addresses[0].port() {
                return Err(AxError::InvalidInput);
            }
            if self.conflict(*address) {
                return Err(AxError::AddrInUse);
            }
        }
        let mut state = self.inner.state.lock();
        if state.local.is_none() {
            state.local = Some(addresses[0]);
            state.vtag = self.dispatcher.random()
        }
        for address in addresses {
            if !state.local_paths.contains(address) {
                state
                    .local_paths
                    .try_reserve(1)
                    .map_err(|_| AxError::NoMemory)?;
                state.local_paths.push(*address)
            }
        }
        drop(state);
        for address in addresses {
            self.send_asconf_change(true, *address)?;
        }
        Ok(())
    }
    /// SCTP_CONNECTX records alternate peer paths before sending INIT on the
    /// first route.  Timer-driven failover selects the next verified address.
    pub fn connectx(&self, addresses: &[SocketAddr]) -> AxResult<i32> {
        let peer = *addresses.first().ok_or(AxError::InvalidInput)?;
        if addresses
            .iter()
            .any(|address| address.port() != peer.port() || address.is_ipv4() != peer.is_ipv4())
        {
            return Err(AxError::InvalidInput);
        }
        self.connect(SocketAddrEx::Ip(peer))?;
        let mut state = self.inner.state.lock();
        for address in addresses {
            if !state.peer_paths.contains(address) {
                state
                    .peer_paths
                    .try_reserve(1)
                    .map_err(|_| AxError::NoMemory)?;
                state.peer_paths.push(*address)
            }
        }
        // Linux's SCTP_SOCKOPT_CONNECTX returns a positive association id.
        // This one-to-one transport uses its local verification tag as the
        // stable association identity, reserving zero and the sign bit.
        Ok((state.vtag & 0x7fff_ffff).max(1) as i32)
    }
    pub fn association_paths(&self) -> (Vec<SocketAddr>, Vec<SocketAddr>) {
        let state = self.inner.state.lock();
        (state.local_paths.clone(), state.peer_paths.clone())
    }
}
impl Configurable for SctpSocket {
    fn nonblocking(&self) -> bool {
        self.inner.general.nonblocking()
    }
    fn get_option_inner(&self, o: &mut GetSocketOption) -> AxResult<bool> {
        self.inner.general.get_option_inner(o)
    }
    fn set_option_inner(&self, o: SetSocketOption) -> AxResult<bool> {
        self.inner.general.set_option_inner(o)
    }
}
impl SocketOps for SctpSocket {
    fn bind(&self, a: SocketAddrEx) -> AxResult {
        let mut a = Self::addr(a)?;
        if a.port() == 0 {
            a.set_port(self.inner.stack.tcp_ephemeral_port()?)
        }
        if self.conflict(a) {
            return Err(AxError::AddrInUse);
        }
        let mut s = self.inner.state.lock();
        if s.local.is_some() {
            return Err(AxError::InvalidInput);
        }
        s.local = Some(a);
        s.local_paths.push(a);
        s.vtag = self.dispatcher.random();
        Ok(())
    }
    fn connect(&self, p: SocketAddrEx) -> AxResult {
        let p = Self::addr(p)?;
        let mut s = self.inner.state.lock();
        if s.phase != Phase::Idle {
            return Err(AxError::AlreadyConnected);
        }
        if s.local.is_none() {
            let source = self
                .inner
                .stack
                .route_source_address(p.ip())
                .ok_or(AxError::NoSuchDevice)?;
            s.local = Some(SocketAddr::new(
                source,
                self.inner.stack.tcp_ephemeral_port()?,
            ));
            let local = s.local.expect("local address just assigned");
            s.local_paths.push(local);
            s.vtag = self.dispatcher.random()
        }
        s.peer = Some(p);
        s.peer_paths.push(p);
        s.phase = Phase::CookieWait;
        s.t1 = axhal::time::wall_time_nanos();
        let (l, tag, tsn, peer_paths) =
            (s.local.unwrap(), s.vtag, s.next_tsn, s.peer_paths.clone());
        drop(s);
        let mut b = Vec::new();
        b.extend_from_slice(&tag.to_be_bytes());
        b.extend_from_slice(&262144u32.to_be_bytes());
        b.extend_from_slice(&10u16.to_be_bytes());
        b.extend_from_slice(&10u16.to_be_bytes());
        b.extend_from_slice(&tsn.to_be_bytes());
        SctpDispatcher::append_forward_tsn_extension(&mut b);
        SctpDispatcher::append_address_parameters(&mut b, &peer_paths);
        let mut w = SctpDispatcher::head(l.port(), p.port(), 0);
        SctpDispatcher::chunk(&mut w, INIT, 0, &b)?;
        {
            let mut state = self.inner.state.lock();
            if state.phase != Phase::CookieWait || state.peer != Some(p) {
                return Err(AxError::WouldBlock);
            }
            state.t1_generation = state.t1_generation.wrapping_add(1);
            state.t1_retries = 0;
            state.t1_wire = w.clone();
            state.t1 = axhal::time::wall_time_nanos();
        }
        self.dispatcher.emit(self.inner.family, l, p, &w)
    }
    fn listen(&self, n: usize) -> AxResult {
        let mut s = self.inner.state.lock();
        if s.local.is_none() || s.phase != Phase::Idle {
            return Err(AxError::InvalidInput);
        }
        s.phase = Phase::Listen;
        s.backlog = cmp::max(n, 1);
        Ok(())
    }
    fn accept(&self) -> AxResult<Socket> {
        self.prepare_accept()?.commit()
    }
    fn send(&self, r: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        self.send_with_metadata(r, options, SctpSendMetadata::default())
    }
    fn recv(&self, w: impl Write + IoBufMut, o: RecvOptions<'_>) -> AxResult<usize> {
        let mut ignored = None;
        self.recv_with_metadata(w, o, &mut ignored)
    }
    fn local_addr(&self) -> AxResult<SocketAddrEx> {
        Ok(SocketAddrEx::Ip(
            self.inner.state.lock().local.ok_or(AxError::NotFound)?,
        ))
    }
    fn peer_addr(&self) -> AxResult<SocketAddrEx> {
        Ok(SocketAddrEx::Ip(
            self.inner.state.lock().peer.ok_or(AxError::NotConnected)?,
        ))
    }
    fn shutdown(&self, h: Shutdown) -> AxResult {
        let mut cancelled_handshake = false;
        {
            let mut state = self.inner.state.lock();
            if h.has_write() && matches!(state.phase, Phase::CookieWait | Phase::CookieEchoed) {
                state.phase = Phase::Closed;
                state.t1 = 0;
                state.t1_retries = 0;
                state.t1_generation = state.t1_generation.wrapping_add(1);
                state.t1_wire.clear();
                cancelled_handshake = true;
            }
        }
        if h.has_read() {
            self.inner.rd.store(true, Ordering::Release)
        }
        if h.has_write() {
            self.inner.wr.store(true, Ordering::Release);
            if !cancelled_handshake {
                let t = self.inner.state.lock().peer_tag;
                let _ = self.dispatcher.send(&self.inner, SHUTDOWN, 0, &[], t);
            }
        }
        self.inner.poll.wake();
        Ok(())
    }
}
impl SctpSocket {
    pub fn send_with_metadata(
        &self,
        mut r: impl Read + IoBuf,
        _: SendOptions,
        metadata: SctpSendMetadata,
    ) -> AxResult<usize> {
        self.dispatcher.drain();
        self.dispatcher.timers(&self.inner);
        // Drain can process ABORT; consume its resulting SO_ERROR before
        // source inspection or payload allocation.
        self.inner.general.consume_pending_error()?;
        if metadata.pr_policy & !0x0030 != 0 {
            return Err(AxError::InvalidInput);
        }
        let n = r.remaining();
        if n > 65536 {
            return Err(AxError::OutOfRange);
        }
        let mut d = vec![0; n];
        r.read_exact(&mut d)?;
        let mut s = self.inner.state.lock();
        if s.phase != Phase::Established {
            return Err(AxError::NotConnected);
        }
        if self.inner.wr.load(Ordering::Acquire) {
            return Err(AxError::BrokenPipe);
        }
        let ssn = s.ssn;
        let p = s.peer.unwrap();
        let l = s
            .local_paths
            .iter()
            .copied()
            .find(|candidate| candidate.is_ipv4() == p.is_ipv4())
            .unwrap_or(s.local.unwrap());
        let tag = s.peer_tag;
        if n > s.rwnd as usize || s.outstanding_bytes.saturating_add(n as u32) > s.cwnd {
            return Err(AxError::WouldBlock);
        }
        s.ssn = s.ssn.wrapping_add(1);
        let mid = s.mid;
        s.mid = s.mid.wrapping_add(1);
        let use_idata = s.peer_idata_supported;
        let mut wires = Vec::new();
        let fragment_payload = 1184usize;
        for (index, fragment) in d.chunks(fragment_payload).enumerate() {
            let t = s.next_tsn;
            s.next_tsn = s.next_tsn.wrapping_add(1);
            let mut b = Vec::new();
            b.extend_from_slice(&t.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&metadata.stream.to_be_bytes());
            if use_idata {
                b.extend_from_slice(&mid.to_be_bytes());
                b.extend_from_slice(
                    &(if index == 0 {
                        metadata.ppid
                    } else {
                        index as u32
                    })
                    .to_be_bytes(),
                );
            } else {
                b.truncate(4);
                b.extend_from_slice(&metadata.stream.to_be_bytes());
                b.extend_from_slice(&ssn.to_be_bytes());
                b.extend_from_slice(&metadata.ppid.to_be_bytes());
            }
            b.extend_from_slice(fragment);
            let mut wire = SctpDispatcher::head(l.port(), p.port(), tag);
            let begin = u8::from(index == 0) << 1;
            let end = u8::from((index + 1) * fragment_payload >= d.len());
            SctpDispatcher::chunk(
                &mut wire,
                if use_idata { I_DATA } else { DATA },
                begin | end | ((metadata.flags & 1) as u8),
                &b,
            )?;
            s.out.push_back(Sent {
                tsn: t,
                wire: wire.clone(),
                when: axhal::time::wall_time_nanos(),
                retries: 0,
                created: axhal::time::wall_time_nanos(),
                pr_policy: metadata.pr_policy,
                pr_value: metadata.pr_value,
                stream: metadata.stream,
                ssn,
                bytes: fragment.len() as u32,
                misses: 0,
            });
            s.outstanding_bytes = s.outstanding_bytes.saturating_add(fragment.len() as u32);
            wires.push(wire);
        }
        s.t3 = axhal::time::wall_time_nanos();
        drop(s);
        for wire in wires {
            self.dispatcher.emit(self.inner.family, l, p, &wire)?;
        }
        Ok(n)
    }
    pub fn recv_with_metadata(
        &self,
        mut w: impl Write + IoBufMut,
        o: RecvOptions<'_>,
        metadata: &mut Option<SctpRecvMetadata>,
    ) -> AxResult<usize> {
        self.dispatcher.drain();
        self.dispatcher.timers(&self.inner);
        // Both raw drain and T1 timers are local fault producers. Their
        // freshly published error takes precedence over EOF/queue/usercopy.
        self.inner.general.consume_pending_error()?;
        let mut s = self.inner.state.lock();
        let Some(r) = s.rx.front().cloned() else {
            return if self.inner.rd.load(Ordering::Acquire) {
                Ok(0)
            } else {
                Err(AxError::WouldBlock)
            };
        };
        let next = s.rx.get(1).map(|next| SctpNextMetadata {
            stream: next.stream,
            flags: next.flags,
            ppid: next.ppid,
            length: next.data.len().min(u32::MAX as usize) as u32,
            assoc_id: 0,
        });
        let cumtsn = s.cum;
        if !o.flags.contains(RecvFlags::PEEK) {
            s.rx.pop_front();
            s.queued_bytes = s.queued_bytes.saturating_sub(r.data.len() as u32);
        }
        drop(s);
        if let Some(a) = o.from {
            *a = SocketAddrEx::Ip(r.peer)
        }
        *metadata = Some(SctpRecvMetadata {
            stream: r.stream,
            ssn: r.ssn,
            flags: r.flags,
            ppid: r.ppid,
            tsn: r.tsn,
            cumtsn,
            context: r.context,
            assoc_id: 0,
            next,
        });
        let n = w.write(&r.data)?;
        Ok(if o.flags.contains(RecvFlags::TRUNCATE) {
            r.data.len()
        } else {
            n
        })
    }
}
impl Pollable for SctpSocket {
    fn poll(&self) -> IoEvents {
        // Polling either raw protocol-132 endpoint also drives NetStack's
        // device poll; drain then consumes every ready SCTP datagram before
        // exposing this association's readiness.
        let _ = self.dispatcher.v4.poll();
        let _ = self.dispatcher.v6.poll();
        self.dispatcher.drain();
        self.dispatcher.timers(&self.inner);
        let s = self.inner.state.lock();
        let mut e = IoEvents::empty();
        e.set(
            IoEvents::READABLE,
            !s.rx.is_empty()
                || (s.phase == Phase::Listen && !s.pending.is_empty())
                || self.inner.rd.load(Ordering::Acquire),
        );
        e.set(
            IoEvents::WRITABLE,
            s.phase == Phase::Established && !self.inner.wr.load(Ordering::Acquire),
        );
        e.set(IoEvents::READ_HANGUP, self.inner.rd.load(Ordering::Acquire));
        drop(s);
        self.inner.general.add_pending_error_event(e)
    }
    fn register<'a>(
        &'a self,
        c: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        // Arm before publishing all sources, then the poller immediately
        // rechecks timers after registration; this closes the deadline wake
        // window used by blocking GeneralOptions transfers.
        self.arm_t1_timer(c)?;
        // inner state + each raw socket's local wake source and NetStack
        // device readiness registration.
        let mut p = PreparedPollRegistration::try_new(5)?;
        p.arm(&self.inner.poll, c.waker())?;
        self.dispatcher.v4.arm_dispatcher_readiness(&mut p, c)?;
        self.dispatcher.v6.arm_dispatcher_readiness(&mut p, c)?;
        p.commit()
    }
}
