//! Namespace-local DCCP protocol-33 dispatcher and socket endpoints.
use crate::{
    NetStack, RecvFlags, RecvOptions, SendOptions, Shutdown, Socket, SocketAddrEx, SocketOps,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, SetSocketOption, SocketFault},
    raw::{RawSocket, RawSocketFamily},
};
use alloc::{
    boxed::Box,
    collections::VecDeque,
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
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::{Context, Poll},
    time::Duration,
};
pub const IPPROTO_DCCP: u8 = 33;
const REQUEST: u8 = 0;
const RESPONSE: u8 = 1;
const DATA: u8 = 2;
const ACK: u8 = 3;
const DATAACK: u8 = 4;
const CLOSEREQ: u8 = 5;
const CLOSE: u8 = 6;
const RESET: u8 = 7;
const SYNC: u8 = 8;
const SYNCACK: u8 = 9;
const MAX: usize = 256;
const RTO: u64 = 3_000_000_000;
const RTO_MAX: u64 = 64_000_000_000;
const TIMEWAIT: u64 = 60_000_000_000;
const SEQ_MASK: u64 = (1 << 48) - 1;
const SEQ_HALF: u64 = 1 << 47;
static NEXT: AtomicU64 = AtomicU64::new(1);
#[derive(Copy, Clone, Eq, PartialEq)]
enum Phase {
    Idle,
    Listen,
    Request,
    Respond,
    Open,
    Closing,
    TimeWait,
    Closed,
}
#[derive(Clone)]
struct Sent {
    wire: Vec<u8>,
    when: u64,
    retries: u8,
    typ: u8,
    seq: u64,
}
struct Queued {
    payload: Vec<u8>,
    priority: u32,
}
struct State {
    // `bound_local` is the address reservation created by bind(2).  `local`
    // may be a route-selected concrete source while a wildcard reservation is
    // connected, but disconnect must restore the reservation rather than turn
    // it into a permanent concrete bind.
    bound_local: Option<SocketAddr>,
    // SO_REUSEADDR is an admission-time property of a local-port
    // reservation.  Do not read the mutable socket option while replacing a
    // route: later setsockopt calls must not retroactively change an already
    // published reservation.
    bound_reuse_address: Option<bool>,
    local: Option<SocketAddr>,
    peer: Option<SocketAddr>,
    phase: Phase,
    service: u32,
    ccid: u8,
    peer_ccid: u8,
    backlog: usize,
    pending: VecDeque<DccpSocket>,
    embryos: VecDeque<DccpSocket>,
    rx: VecDeque<(SocketAddr, Vec<u8>)>,
    tx: VecDeque<Queued>,
    flight: u32,
    cwnd: u32,
    seq: u64,
    gsr: u64,
    iss: u64,
    short_seq: bool,
    send_cov: u8,
    recv_cov: u8,
    qpolicy: u32,
    qmax: u32,
    server_timewait: u64,
    out: VecDeque<Sent>,
    deadline: u64,
    last_error: u8,
    filter: Option<Arc<dyn crate::SocketFilter>>,
}
impl State {
    fn new() -> Self {
        let iss = NEXT.fetch_add(1, Ordering::Relaxed) & 0xffff_ffff_ffff;
        Self {
            bound_local: None,
            bound_reuse_address: None,
            local: None,
            peer: None,
            phase: Phase::Idle,
            service: 0,
            ccid: 2,
            peer_ccid: 2,
            backlog: 0,
            pending: VecDeque::new(),
            embryos: VecDeque::new(),
            rx: VecDeque::new(),
            tx: VecDeque::new(),
            flight: 0,
            cwnd: 4,
            seq: iss,
            gsr: 0,
            iss,
            short_seq: false,
            send_cov: 0,
            recv_cov: 0,
            qpolicy: 0,
            qmax: 0,
            server_timewait: TIMEWAIT,
            out: VecDeque::new(),
            deadline: 0,
            last_error: 0,
            filter: None,
        }
    }
}

/// Socket-level state inherited by a passive child.  Keep this distinct from
/// transport progress: a child starts a fresh response handshake, but Linux
/// clones the listener's socket options and packet filter into it.
struct PassiveSnapshot {
    reuse_address: bool,
    dont_route: bool,
    send_timeout: Duration,
    recv_timeout: Duration,
    ccid: u8,
    short_seq: bool,
    send_cov: u8,
    recv_cov: u8,
    qpolicy: u32,
    qmax: u32,
    server_timewait: u64,
    filter: Option<Arc<dyn crate::SocketFilter>>,
}
#[derive(Clone)]
struct Packet {
    local: SocketAddr,
    peer: SocketAddr,
    typ: u8,
    seq: u64,
    ack: Option<u64>,
    service: Option<u32>,
    reset: Option<u8>,
    options: Vec<u8>,
    payload: Vec<u8>,
    coverage: usize,
    wire_len: usize,
    header_words: usize,
}
fn now() -> u64 {
    axhal::time::wall_time_nanos()
}
fn sum16(mut sum: u32, b: &[u8]) -> u32 {
    let mut i = 0;
    while i + 1 < b.len() {
        sum += u16::from_be_bytes([b[i], b[i + 1]]) as u32;
        i += 2
    }
    if i < b.len() {
        sum += (b[i] as u32) << 8
    }
    sum
}
fn fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16)
    }
    !(sum as u16)
}
fn pseudo(src: IpAddr, dst: IpAddr, len: usize) -> u32 {
    match (src, dst) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let mut s = sum16(0, &a.octets());
            s = sum16(s, &b.octets());
            s += 33 + len as u32;
            s
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let mut s = sum16(0, &a.octets());
            s = sum16(s, &b.octets());
            s += (len as u32 >> 16) + (len as u32 & 0xffff) + 33;
            s
        }
        _ => 0,
    }
}
/// RFC 4340 sequence arithmetic: sequence values live in a 48-bit ring and
/// comparisons are defined only inside one half-space.
fn seq_after(a: u64, b: u64) -> bool {
    let delta = a.wrapping_sub(b) & SEQ_MASK;
    delta != 0 && delta < SEQ_HALF
}
fn seq_acked(sequence: u64, acknowledgement: u64) -> bool {
    sequence == acknowledgement || seq_after(acknowledgement, sequence)
}
struct QueuedIngress {
    epoch: u64,
    raw: Vec<u8>,
}
struct Ingress {
    id: u64,
    epoch: AtomicU64,
    q: Arc<Mutex<VecDeque<QueuedIngress>>>,
    wake: Arc<PollSet>,
}
impl Ingress {
    fn new() -> AxResult<Self> {
        Ok(Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            epoch: AtomicU64::new(1),
            q: Arc::try_new(Mutex::new(VecDeque::new())).map_err(|_| AxError::NoMemory)?,
            wake: Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?,
        })
    }
    fn replace_route(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.q.lock().clear();
    }
    fn push(&self, epoch: u64, raw: Vec<u8>) {
        if self.epoch.load(Ordering::Acquire) != epoch {
            return;
        }
        let mut q = self.q.lock();
        if self.epoch.load(Ordering::Acquire) == epoch && q.len() < MAX {
            q.push_back(QueuedIngress { epoch, raw });
            self.wake.wake();
        }
    }
}
struct Route {
    ep: Weak<Ingress>,
    id: u64,
    epoch: u64,
    local: SocketAddr,
    reservation: SocketAddr,
    peer: Option<SocketAddr>,
    listen: bool,
    // Keep the admission property with the reservation, rather than looking
    // at the current socket state after a route is replaced.  A connected
    // route remains a local-port owner and must therefore participate in the
    // same SO_REUSEADDR decision as an ordinary bound endpoint.
    reuse_address: bool,
    // Passive children carry the listener ingress identity that owns their
    // shared local-port reservation.  Besides conflict admission, this lets a
    // listener tear down its unaccepted children and replace its own route in
    // one dispatcher-table transaction.
    reservation_owner: Option<u64>,
}
struct DccpTimer {
    deadline: TimeValue,
    future: Pin<Box<dyn Future<Output = Result<(), TimerRegistrationError>> + Send>>,
}
pub struct DccpDispatcher {
    v4: RawSocket,
    v6: RawSocket,
    routes: Mutex<Vec<Route>>,
    ephemeral_port: Mutex<u16>,
}
impl DccpDispatcher {
    pub(crate) fn try_new(s: Arc<NetStack>) -> AxResult<Arc<Self>> {
        let v4 = RawSocket::new(s.clone(), RawSocketFamily::Ipv4, 33)?;
        let v6 = RawSocket::new(s.clone(), RawSocketFamily::Ipv6, 33)?;
        v4.set_header_included(true);
        v6.set_header_included(true);
        v4.set_dispatcher_device_mask(u64::MAX);
        v6.set_dispatcher_device_mask(u64::MAX);
        Arc::try_new(Self {
            v4,
            v6,
            routes: Mutex::new(Vec::new()),
            ephemeral_port: Mutex::new(0xc000),
        })
        .map_err(|_| AxError::NoMemory)
    }
    fn install(
        &self,
        e: &Arc<Ingress>,
        l: SocketAddr,
        p: Option<SocketAddr>,
        reservation: SocketAddr,
        listen: bool,
        reuse: bool,
        reservation_owner: Option<u64>,
    ) -> AxResult {
        let mut r = self.routes.lock();
        r.retain(|x| x.ep.strong_count() != 0);
        // A listener is unique even when SO_REUSEADDR is enabled.  Accepted
        // children are not listeners and deliberately share this listener's
        // reservation below.
        if listen
            && r.iter().any(|x| {
                x.id != e.id
                    && x.listen
                    && x.reservation.port() == reservation.port()
                    && (x.reservation.ip().is_unspecified()
                        || reservation.ip().is_unspecified()
                        || x.reservation.ip() == reservation.ip())
            })
        {
            return Err(AxError::AddrInUse);
        }
        // A passive child may share only a live listener's reservation.  The
        // listener route is the authority for its children: this admits both
        // child-to-listener and sibling-child sharing, but a stale owner ID
        // cannot weaken ordinary local-port conflict admission after teardown.
        let live_listener_owner = reservation_owner
            .filter(|owner| r.iter().any(|route| route.id == *owner && route.listen));
        // A connected route still owns a local-port reservation.  Do this
        // independently of the peer tuple check: otherwise a peer-specific
        // route can accidentally escape SO_REUSEADDR conflict admission.
        if r.iter().any(|x| {
            let shares_listener_reservation = live_listener_owner.is_some_and(|owner| {
                // Installing a passive child: the existing listener owns its
                // reservation, and every child carrying that same live owner
                // borrows it too.
                x.id == owner || x.reservation_owner == Some(owner)
            })
                // Re-listening after disconnect: accepted children retained
                // by this endpoint still borrow the same reservation.
                || (listen && x.reservation_owner == Some(e.id));
            x.id != e.id
                && !shares_listener_reservation
                // SO_REUSEADDR only relaxes a port conflict when *both*
                // reservations opted in.  In particular, replacing an
                // unconnected route by a peer-specific connected route must
                // never make a non-reuse binder lose its reservation.
                && (!reuse || !x.reuse_address)
                && x.reservation.port() == reservation.port()
                && (x.reservation.ip().is_unspecified()
                    || reservation.ip().is_unspecified()
                    || x.reservation.ip() == reservation.ip())
        }) {
            return Err(AxError::AddrInUse);
        }
        if p.is_some()
            && r.iter()
                .any(|x| x.id != e.id && x.local == l && x.peer == p)
        {
            return Err(AxError::AddrInUse);
        }
        // Replacing this endpoint consumes the slot its old route already
        // owns; only a genuinely new endpoint needs another allocation.
        if !r.iter().any(|x| x.id == e.id) {
            r.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        }
        // Replace the endpoint generation only once admission and allocation
        // are known to succeed.  This keeps a prior route live on a failed
        // replacement, while invalidating packets captured from its old
        // generation.
        e.replace_route();
        r.retain(|x| x.id != e.id);
        r.push(Route {
            ep: Arc::downgrade(e),
            id: e.id,
            epoch: e.epoch.load(Ordering::Acquire),
            local: l,
            reservation,
            peer: p,
            listen,
            reuse_address: reuse,
            reservation_owner,
        });
        Ok(())
    }
    /// Replace a listener with an ordinary bound route while removing every
    /// unaccepted child route which borrows that listener's reservation.  The
    /// admission check and route-table update share one lock, so a failed
    /// replacement leaves both the listener and its children reachable.
    fn replace_listener_with_bound(
        &self,
        e: &Arc<Ingress>,
        local: SocketAddr,
        reuse: bool,
        unaccepted_children: &[u64],
    ) -> AxResult {
        let mut r = self.routes.lock();
        r.retain(|x| x.ep.strong_count() != 0);
        if r.iter().any(|x| {
            x.id != e.id
                && x.reservation_owner != Some(e.id)
                && (!reuse || !x.reuse_address)
                && x.reservation.port() == local.port()
                && (x.reservation.ip().is_unspecified()
                    || local.ip().is_unspecified()
                    || x.reservation.ip() == local.ip())
        }) {
            return Err(AxError::AddrInUse);
        }
        if !r.iter().any(|x| x.id == e.id) {
            r.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        }
        e.replace_route();
        r.retain(|x| x.id != e.id && !unaccepted_children.contains(&x.id));
        r.push(Route {
            ep: Arc::downgrade(e),
            id: e.id,
            epoch: e.epoch.load(Ordering::Acquire),
            local,
            reservation: local,
            peer: None,
            listen: false,
            reuse_address: reuse,
            reservation_owner: None,
        });
        Ok(())
    }
    fn next_ephemeral_port(&self) -> u16 {
        let mut port = self.ephemeral_port.lock();
        let candidate = *port;
        *port = if candidate == u16::MAX {
            0xc000
        } else {
            candidate + 1
        };
        candidate
    }
    /// Atomically make an ephemeral port available to DCCP by publishing its
    /// route reservation.  DCCP owns both the candidate sequence and the
    /// admission table, rather than borrowing TCP's unrelated port table.
    fn install_ephemeral(
        &self,
        e: &Arc<Ingress>,
        source: IpAddr,
        peer: Option<SocketAddr>,
        listen: bool,
        reuse: bool,
    ) -> AxResult<SocketAddr> {
        // Keep trying for the entire DCCP ephemeral range before reporting
        // exhaustion; each candidate is admitted through `install` above.
        for _ in 0..=(u16::MAX - 0xc000) {
            let local = SocketAddr::new(source, self.next_ephemeral_port());
            if peer.is_some_and(|peer| local.port() == peer.port()) {
                continue;
            }
            match self.install(e, local, peer, local, listen, reuse, None) {
                Ok(()) => return Ok(local),
                Err(AxError::AddrInUse) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(AxError::AddrInUse)
    }
    fn remove(&self, e: &Ingress) {
        self.routes.lock().retain(|x| x.id != e.id)
    }
    fn packet(p: &[u8]) -> Option<Packet> {
        let (h, end, src, dst) = match p.first()? >> 4 {
            4 => {
                if p.len() < 20 {
                    return None;
                }
                let ihl = ((p[0] & 15) as usize).checked_mul(4)?;
                let total = u16::from_be_bytes([p[2], p[3]]) as usize;
                if ihl < 20
                    || ihl > p.len()
                    || total < ihl
                    || total > p.len()
                    || p[9] != IPPROTO_DCCP
                    || fold(sum16(0, &p[..ihl])) != 0
                {
                    return None;
                }
                (
                    ihl,
                    total,
                    IpAddr::V4(<[u8; 4]>::try_from(&p[12..16]).ok()?.into()),
                    IpAddr::V4(<[u8; 4]>::try_from(&p[16..20]).ok()?.into()),
                )
            }
            6 => {
                if p.len() < 40 {
                    return None;
                }
                let total = 40usize.checked_add(u16::from_be_bytes([p[4], p[5]]) as usize)?;
                if total > p.len() || p[6] != IPPROTO_DCCP {
                    return None;
                }
                (
                    40,
                    total,
                    IpAddr::V6(<[u8; 16]>::try_from(&p[8..24]).ok()?.into()),
                    IpAddr::V6(<[u8; 16]>::try_from(&p[24..40]).ok()?.into()),
                )
            }
            _ => return None,
        };
        let b = &p[h..end];
        if b.len() < 12 {
            return None;
        }
        let words = b[4] as usize;
        let hl = words.checked_mul(4)?;
        if words < 3 || hl > b.len() {
            return None;
        }
        let cov = if b[5] & 15 == 0 {
            b.len()
        } else {
            (words + (b[5] & 15) as usize - 1).checked_mul(4)?
        };
        // The pseudo-header always carries the complete DCCP length; CsCov
        // limits only the transport bytes included in the checksum sum.
        if cov > b.len() || fold(sum16(pseudo(src, dst, b.len()), &b[..cov])) != 0 {
            return None;
        }
        let typ = (b[8] >> 1) & 15;
        if typ > SYNCACK {
            return None;
        }
        let short = b[8] & 1 == 0;
        let seq = if short {
            ((b[9] as u64) << 16) | ((b[10] as u64) << 8) | b[11] as u64
        } else {
            if hl < 16 || b[12] != 0 {
                return None;
            }
            ((b[9] as u64) << 40)
                | ((b[10] as u64) << 32)
                | ((b[11] as u64) << 24)
                | ((b[13] as u64) << 16)
                | ((b[14] as u64) << 8)
                | b[15] as u64
        };
        let mut at = if short { 12 } else { 16 };
        let mut ack = None;
        let mut service = None;
        let mut reset = None;
        if typ == REQUEST {
            if at + 4 > hl {
                return None;
            }
            service = Some(u32::from_be_bytes(b[at..at + 4].try_into().ok()?));
            at += 4
        } else if typ == RESPONSE {
            if at + 12 > hl {
                return None;
            }
            ack = Some(u64::from_be_bytes([
                0,
                0,
                b[at + 2],
                b[at + 3],
                b[at + 4],
                b[at + 5],
                b[at + 6],
                b[at + 7],
            ]));
            service = Some(u32::from_be_bytes(b[at + 8..at + 12].try_into().ok()?));
            at += 12
        } else if typ == RESET {
            if at + 12 > hl {
                return None;
            }
            ack = Some(u64::from_be_bytes([
                0,
                0,
                b[at + 2],
                b[at + 3],
                b[at + 4],
                b[at + 5],
                b[at + 6],
                b[at + 7],
            ]));
            reset = Some(b[at + 8]);
            at += 12
        } else if typ != DATA {
            if at + 8 > hl {
                return None;
            }
            ack = Some(u64::from_be_bytes([
                0,
                0,
                b[at + 2],
                b[at + 3],
                b[at + 4],
                b[at + 5],
                b[at + 6],
                b[at + 7],
            ]));
            at += 8
        }
        Some(Packet {
            local: SocketAddr::new(dst, u16::from_be_bytes([b[2], b[3]])),
            peer: SocketAddr::new(src, u16::from_be_bytes([b[0], b[1]])),
            typ,
            seq,
            ack,
            service,
            reset,
            options: b[at..hl].to_vec(),
            payload: b[hl..].to_vec(),
            coverage: cov,
            wire_len: b.len(),
            header_words: words,
        })
    }
    pub fn drain(&self) {
        for raw in [&self.v4, &self.v6] {
            for _ in 0..64 {
                let mut p = vec![0; 65535];
                let n = match raw.recv(
                    &mut p[..],
                    RecvOptions {
                        flags: RecvFlags::DONT_WAIT,
                        ..Default::default()
                    },
                ) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                p.truncate(n);
                let Some(pkt) = Self::packet(&p) else {
                    continue;
                };
                let r = self.routes.lock();
                let hit = r
                    .iter()
                    .find_map(|x| {
                        x.ep.upgrade()
                            .filter(|_| x.local == pkt.local && x.peer == Some(pkt.peer))
                            .map(|endpoint| (endpoint, x.epoch))
                    })
                    .or_else(|| {
                        r.iter().find_map(|x| {
                            x.ep.upgrade()
                                .filter(|_| {
                                    x.listen
                                        && pkt.typ == REQUEST
                                        && x.local.port() == pkt.local.port()
                                        && (x.local.ip().is_unspecified()
                                            || x.local.ip() == pkt.local.ip())
                                })
                                .map(|endpoint| (endpoint, x.epoch))
                        })
                    });
                drop(r);
                if let Some((endpoint, epoch)) = hit {
                    endpoint.push(epoch, p)
                }
            }
        }
    }
    fn emit(&self, f: RawSocketFamily, p: &[u8], to: SocketAddr, dont_route: bool) -> AxResult {
        match f {
            RawSocketFamily::Ipv4 => self
                .v4
                .send_header_included_with_dont_route(p, to, dont_route),
            RawSocketFamily::Ipv6 => self
                .v6
                .send_header_included_with_dont_route(p, to, dont_route),
        }
    }
}
pub struct DccpSocket {
    stack: Arc<NetStack>,
    family: RawSocketFamily,
    dispatcher: Arc<DccpDispatcher>,
    ingress: Arc<Ingress>,
    general: GeneralOptions,
    // Serializes replacement/removal of the ingress route with state
    // publication and packet processing.  In particular, disconnect cannot
    // race a first REQUEST or let an already-dequeued packet revive a reset
    // socket.
    lifecycle: Mutex<()>,
    state: Mutex<State>,
    rd: AtomicBool,
    wr: AtomicBool,
    poll: PollSet,
    timer: Mutex<Option<DccpTimer>>,
    accept: u64,
}
pub struct DccpAcceptReservation<'a> {
    listener: &'a DccpSocket,
    peer: SocketAddr,
    token: u64,
}
impl DccpAcceptReservation<'_> {
    pub fn identity(&self) -> SocketAddr {
        self.peer
    }
    pub fn commit(self) -> AxResult<Socket> {
        let mut s = self.listener.state.lock();
        let i = s
            .pending
            .iter()
            .position(|x| x.accept == self.token)
            .ok_or(AxError::WouldBlock)?;
        Ok(Socket::Dccp(s.pending.remove(i).unwrap()))
    }
}
impl DccpSocket {
    pub fn new(stack: Arc<NetStack>, family: RawSocketFamily) -> AxResult<Self> {
        let d = stack.dccp_dispatcher()?;
        Ok(Self {
            stack,
            family,
            dispatcher: d,
            ingress: Arc::try_new(Ingress::new()?).map_err(|_| AxError::NoMemory)?,
            general: GeneralOptions::new(),
            lifecycle: Mutex::new(()),
            state: Mutex::new(State::new()),
            rd: AtomicBool::new(false),
            wr: AtomicBool::new(false),
            poll: PollSet::new(),
            timer: Mutex::new(None),
            accept: NEXT.fetch_add(1, Ordering::Relaxed),
        })
    }
    pub(crate) fn recv_pending_len(&self) -> AxResult<usize> {
        self.drive()?;
        Ok(self.state.lock().rx.front().map_or(0, |x| x.1.len()))
    }
    pub(crate) fn retry_transfer<T>(
        &self,
        direction: crate::SocketTransferDirection,
        effective_nonblocking: bool,
        attempt: &mut impl FnMut() -> AxResult<T>,
    ) -> AxResult<T> {
        match direction {
            crate::SocketTransferDirection::Receive => self
                .general
                .recv_poller_data_first_with_effective_nonblocking(
                    self,
                    effective_nonblocking,
                    attempt,
                ),
            crate::SocketTransferDirection::Send => {
                self.general
                    .transfer_poller(self, direction, effective_nonblocking, attempt)
            }
        }
    }
    pub fn service_code(&self) -> u32 {
        self.state.lock().service
    }
    pub fn set_service_code(&self, v: u32) -> AxResult {
        let mut s = self.state.lock();
        if s.phase != Phase::Idle {
            return Err(AxError::InvalidInput);
        }
        s.service = v;
        Ok(())
    }
    pub fn ccid(&self) -> u8 {
        self.state.lock().ccid
    }
    pub fn set_ccid(&self, v: u8) -> AxResult {
        if v != 2 && v != 3 {
            return Err(AxError::InvalidInput);
        }
        self.state.lock().ccid = v;
        Ok(())
    }
    pub const fn available_ccids() -> [u8; 2] {
        [2, 3]
    }
    pub fn tx_ccid(&self) -> u8 {
        self.ccid()
    }
    pub fn rx_ccid(&self) -> u8 {
        self.state.lock().peer_ccid
    }
    pub fn packet_size(&self) -> usize {
        65507
    }
    pub fn mps(&self) -> usize {
        65475
    }
    pub fn sequence(&self) -> u64 {
        self.state.lock().seq
    }
    pub fn ccid_info(&self) -> (u8, u8, u32, u32) {
        let s = self.state.lock();
        (s.ccid, s.peer_ccid, s.cwnd, s.flight)
    }
    pub fn qpolicy(&self) -> (u32, u32) {
        let s = self.state.lock();
        (s.qpolicy, s.qmax)
    }
    pub fn set_qpolicy(&self, p: u32, q: u32) -> AxResult {
        if p > 1 {
            return Err(AxError::InvalidInput);
        }
        let mut s = self.state.lock();
        s.qpolicy = p;
        s.qmax = q;
        Ok(())
    }
    pub fn server_timewait(&self) -> u64 {
        self.state.lock().server_timewait
    }
    pub fn set_server_timewait(&self, v: u64) {
        self.state.lock().server_timewait = v
    }
    pub fn send_cscov(&self) -> u8 {
        self.state.lock().send_cov
    }
    pub fn recv_cscov(&self) -> u8 {
        self.state.lock().recv_cov
    }
    pub fn set_send_cscov(&self, v: u8) -> AxResult {
        if v > 15 {
            Err(AxError::InvalidInput)
        } else {
            self.state.lock().send_cov = v;
            Ok(())
        }
    }
    pub fn set_recv_cscov(&self, v: u8) -> AxResult {
        if v > 15 {
            Err(AxError::InvalidInput)
        } else {
            self.state.lock().recv_cov = v;
            Ok(())
        }
    }
    pub fn feature_change(&self, local: bool, feature: u8, v: u8) -> AxResult {
        if feature != 1 || !matches!(v, 2 | 3) {
            return Err(AxError::InvalidInput);
        }
        let mut s = self.state.lock();
        if local {
            s.ccid = v
        } else {
            s.peer_ccid = v
        }
        Ok(())
    }
    pub fn send_with_priority(
        &self,
        s: impl Read + IoBuf,
        _: SendOptions,
        priority: Option<u32>,
    ) -> AxResult<usize> {
        let p = priority.unwrap_or(0);
        if p > 0xffff {
            return Err(AxError::InvalidInput);
        }
        self.send_data(s, p)
    }
    /// Publish a one-shot asynchronous fault before waking all waiters.  The
    /// release store in `GeneralOptions` pairs with the poller's acquisition,
    /// so a wake cannot expose readiness without its corresponding SO_ERROR.
    pub(crate) fn set_pending_error(&self, error: SocketFault) {
        self.general.set_pending_error(error);
        self.poll.wake();
    }
    pub fn set_filter(&self, filter: Option<Arc<dyn crate::SocketFilter>>) -> AxResult {
        self.state.lock().filter = filter;
        Ok(())
    }
    fn passive_snapshot(&self, state: &State) -> PassiveSnapshot {
        PassiveSnapshot {
            reuse_address: self.general.reuse_address(),
            dont_route: self.general.dont_route(),
            send_timeout: self.general.send_timeout().unwrap_or(Duration::ZERO),
            recv_timeout: self.general.recv_timeout().unwrap_or(Duration::ZERO),
            ccid: state.ccid,
            short_seq: state.short_seq,
            send_cov: state.send_cov,
            recv_cov: state.recv_cov,
            qpolicy: state.qpolicy,
            qmax: state.qmax,
            server_timewait: state.server_timewait,
            filter: state.filter.clone(),
        }
    }
    fn apply_passive_snapshot(&self, snapshot: PassiveSnapshot) -> AxResult {
        self.general
            .set_option_inner(SetSocketOption::ReuseAddress(&snapshot.reuse_address))?;
        self.general
            .set_option_inner(SetSocketOption::DontRoute(&snapshot.dont_route))?;
        self.general
            .set_option_inner(SetSocketOption::SendTimeout(&snapshot.send_timeout))?;
        self.general
            .set_option_inner(SetSocketOption::ReceiveTimeout(&snapshot.recv_timeout))?;
        let mut state = self.state.lock();
        state.ccid = snapshot.ccid;
        state.short_seq = snapshot.short_seq;
        state.send_cov = snapshot.send_cov;
        state.recv_cov = snapshot.recv_cov;
        state.qpolicy = snapshot.qpolicy;
        state.qmax = snapshot.qmax;
        state.server_timewait = snapshot.server_timewait;
        state.filter = snapshot.filter;
        Ok(())
    }
    fn features(s: &State) -> Vec<u8> {
        let mut o = Vec::new();
        o.extend_from_slice(&[32, 4, 1, s.ccid]);
        o.extend_from_slice(&[34, 4, 1, s.peer_ccid]);
        o.extend_from_slice(&[32, 4, 2, if s.short_seq { 1 } else { 0 }]);
        while o.len() % 4 != 0 {
            o.push(0)
        }
        o
    }
    fn wire(&self, typ: u8, payload: &[u8], reset: Option<u8>) -> AxResult<(Vec<u8>, SocketAddr)> {
        let mut s = self.state.lock();
        let l = s.local.ok_or(AxError::NotFound)?;
        let peer = s.peer.ok_or(AxError::NotConnected)?;
        s.seq = (s.seq + 1) & 0xffff_ffff_ffff;
        let seq = s.seq;
        let ack = s.gsr;
        let mut d = vec![0; 16];
        d[0..2].copy_from_slice(&l.port().to_be_bytes());
        d[2..4].copy_from_slice(&peer.port().to_be_bytes());
        d[5] = (s.ccid << 4) | s.send_cov;
        d[8] = (typ << 1) | 1;
        d[9] = ((seq >> 40) & 255) as u8;
        d[10] = ((seq >> 32) & 255) as u8;
        d[11] = ((seq >> 24) & 255) as u8;
        d[13] = ((seq >> 16) & 255) as u8;
        d[14] = ((seq >> 8) & 255) as u8;
        d[15] = seq as u8;
        match typ {
            REQUEST => d.extend_from_slice(&s.service.to_be_bytes()),
            RESPONSE => {
                d.extend_from_slice(&[
                    0,
                    0,
                    ((ack >> 40) & 255) as u8,
                    ((ack >> 32) & 255) as u8,
                    ((ack >> 24) & 255) as u8,
                    ((ack >> 16) & 255) as u8,
                    ((ack >> 8) & 255) as u8,
                    ack as u8,
                ]);
                d.extend_from_slice(&s.service.to_be_bytes())
            }
            RESET => d.extend_from_slice(&[
                0,
                0,
                ((ack >> 40) & 255) as u8,
                ((ack >> 32) & 255) as u8,
                ((ack >> 24) & 255) as u8,
                ((ack >> 16) & 255) as u8,
                ((ack >> 8) & 255) as u8,
                ack as u8,
                reset.unwrap_or(0),
                0,
                0,
                0,
            ]),
            DATA => {}
            _ => d.extend_from_slice(&[
                0,
                0,
                ((ack >> 40) & 255) as u8,
                ((ack >> 32) & 255) as u8,
                ((ack >> 24) & 255) as u8,
                ((ack >> 16) & 255) as u8,
                ((ack >> 8) & 255) as u8,
                ack as u8,
            ]),
        }
        d.extend_from_slice(&Self::features(&s));
        d[4] = (d.len() / 4) as u8;
        d.extend_from_slice(payload);
        let dccp_len = u16::try_from(d.len()).map_err(|_| AxError::InvalidInput)?;
        let cov = if s.send_cov == 0 {
            d.len()
        } else {
            ((d[4] as usize + s.send_cov as usize - 1) * 4).min(d.len())
        };
        let c = fold(sum16(pseudo(l.ip(), peer.ip(), d.len()), &d[..cov]));
        d[6..8].copy_from_slice(&c.to_be_bytes());
        let mut ip = match (l.ip(), peer.ip()) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                let mut x = vec![0; 20];
                x[0] = 0x45;
                let total =
                    u16::try_from(20usize.checked_add(d.len()).ok_or(AxError::InvalidInput)?)
                        .map_err(|_| AxError::InvalidInput)?;
                x[2..4].copy_from_slice(&total.to_be_bytes());
                x[8] = 64;
                x[9] = 33;
                x[12..16].copy_from_slice(&a.octets());
                x[16..20].copy_from_slice(&b.octets());
                let checksum = fold(sum16(0, &x));
                x[10..12].copy_from_slice(&checksum.to_be_bytes());
                x
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                let mut x = vec![0; 40];
                x[0] = 0x60;
                x[4..6].copy_from_slice(&dccp_len.to_be_bytes());
                x[6] = 33;
                x[7] = 64;
                x[8..24].copy_from_slice(&a.octets());
                x[24..40].copy_from_slice(&b.octets());
                x
            }
            _ => return Err(AxError::InvalidInput),
        };
        ip.extend_from_slice(&d);
        Ok((ip, peer))
    }
    /// Read SO_DONTROUTE at the output boundary.  In particular, do not
    /// snapshot it at connect time: a later setsockopt must govern both a
    /// new control/data packet and every retransmission.
    fn emit(&self, wire: &[u8], peer: SocketAddr) -> AxResult {
        self.dispatcher
            .emit(self.family, wire, peer, self.general.dont_route())
    }
    fn transmit(&self, typ: u8, payload: &[u8], reset: Option<u8>, reliable: bool) -> AxResult {
        let (wire, peer) = self.wire(typ, payload, reset)?;
        self.emit(&wire, peer)?;
        if reliable {
            let mut s = self.state.lock();
            let seq = s.seq;
            s.out.push_back(Sent {
                wire,
                when: now(),
                retries: 0,
                typ,
                seq,
            });
            s.deadline = now() + RTO
        }
        Ok(())
    }
    fn flush_data(&self) -> AxResult {
        loop {
            let item = {
                let mut s = self.state.lock();
                if s.phase != Phase::Open || s.flight >= s.cwnd {
                    None
                } else {
                    let i = if s.qpolicy == 1 {
                        // Strict `>` keeps the earliest entry among equal
                        // priorities, i.e. PRIO remains FIFO per class.
                        let mut best = 0;
                        for i in 1..s.tx.len() {
                            if s.tx[i].priority > s.tx[best].priority {
                                best = i;
                            }
                        }
                        best
                    } else {
                        0
                    };
                    s.tx.remove(i)
                }
            };
            let Some(item) = item else { return Ok(()) };
            self.transmit(DATA, &item.payload, None, true)?;
            let mut s = self.state.lock();
            s.flight = s.flight.saturating_add(1)
        }
    }
    fn send_data(&self, mut r: impl Read + IoBuf, priority: u32) -> AxResult<usize> {
        // Local write shutdown dominates connection state, including a
        // concurrent reset which has already transitioned the association.
        if self.wr.load(Ordering::Acquire) {
            return Err(AxError::BrokenPipe);
        }
        self.drive()?;
        if self.wr.load(Ordering::Acquire) {
            return Err(AxError::BrokenPipe);
        }
        if self.state.lock().phase != Phase::Open {
            return Err(AxError::NotConnected);
        }
        let n = r.remaining();
        if n > self.mps() {
            return Err(AxError::InvalidInput);
        }
        let mut payload = vec![0; n];
        r.read_exact(&mut payload)?;
        {
            let mut s = self.state.lock();
            if self.wr.load(Ordering::Acquire) {
                return Err(AxError::BrokenPipe);
            }
            if s.phase != Phase::Open {
                return Err(AxError::NotConnected);
            }
            if s.qmax != 0 && s.tx.len() >= s.qmax as usize {
                return Err(AxError::WouldBlock);
            }
            s.tx.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            s.tx.push_back(Queued { payload, priority })
        }
        self.flush_data()?;
        Ok(n)
    }
    fn take_features(s: &mut State, o: &[u8]) {
        let mut i = 0;
        while i < o.len() {
            let kind = o[i];
            if kind == 0 {
                i += 1;
                continue;
            }
            if i + 2 > o.len() {
                break;
            }
            let n = o[i + 1] as usize;
            if n < 3 || i + n > o.len() {
                break;
            }
            if matches!(kind, 32 | 34) && o[i + 2] == 1 && n >= 4 && matches!(o[i + 3], 2 | 3) {
                if kind == 32 {
                    s.ccid = o[i + 3]
                } else {
                    s.peer_ccid = o[i + 3]
                }
            }
            if matches!(kind, 32 | 34) && o[i + 2] == 2 && n >= 4 {
                s.short_seq = o[i + 3] != 0
            }
            i += n
        }
    }
    fn accepts_coverage(s: &State, p: &Packet) -> bool {
        if s.recv_cov == 0 {
            p.coverage == p.wire_len
        } else {
            p.coverage >= ((p.header_words + s.recv_cov as usize - 1) * 4).min(p.wire_len)
        }
    }
    fn next_deadline(&self) -> u64 {
        let s = self.state.lock();
        let mut earliest = s.deadline;
        for embryo in &s.embryos {
            let deadline = embryo.state.lock().deadline;
            if deadline != 0 && (earliest == 0 || deadline < earliest) {
                earliest = deadline;
            }
        }
        earliest
    }
    fn arm_timer(&self, c: &mut Context<'_>) -> Result<(), PollRegistrationError> {
        let deadline = self.next_deadline();
        if deadline == 0 {
            *self.timer.lock() = None;
            return Ok(());
        }
        let deadline = TimeValue::from_micros((deadline / 1_000) as _);
        let mut timer = self.timer.lock();
        if timer.as_ref().is_none_or(|x| x.deadline != deadline) {
            let future =
                Box::try_new(sleep_until(deadline)).map_err(|_| PollRegistrationError::NoMemory)?;
            *timer = Some(DccpTimer {
                deadline,
                future: Box::into_pin(future),
            })
        }
        if let Some(active_timer) = timer.as_mut() {
            match active_timer.future.as_mut().poll(c) {
                Poll::Ready(Ok(())) => {
                    *timer = None;
                    c.waker().wake_by_ref()
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
    fn drive(&self) -> AxResult {
        let _lifecycle = self.lifecycle.lock();
        self.dispatcher.drain();
        let tnow = now();
        {
            let mut s = self.state.lock();
            if s.deadline != 0
                && tnow >= s.deadline
                && matches!(s.phase, Phase::Request | Phase::Respond | Phase::Closing)
            {
                if s.out.front().is_some_and(|x| x.retries >= 8) {
                    s.phase = Phase::Closed;
                    s.last_error = u8::MAX;
                    s.deadline = 0;
                    s.out.clear();
                    self.rd.store(true, Ordering::Release);
                    self.wr.store(true, Ordering::Release);
                    self.set_pending_error(SocketFault::TimedOut)
                } else {
                    let peer = s.peer.expect("an active DCCP retransmission has a peer");
                    if let Some(x) = s.out.front_mut() {
                        self.emit(&x.wire, peer)?;
                        x.retries += 1;
                        x.when = tnow;
                        s.deadline = tnow + (RTO << x.retries.min(4)).min(RTO_MAX);
                    }
                }
            }
            if s.phase == Phase::TimeWait && tnow >= s.deadline {
                s.phase = Phase::Closed;
                s.deadline = 0;
                s.out.clear();
                self.rd.store(true, Ordering::Release);
                self.wr.store(true, Ordering::Release);
                self.poll.wake();
            }
        }
        while let Some(queued) = self.ingress.q.lock().pop_front() {
            if queued.epoch != self.ingress.epoch.load(Ordering::Acquire) {
                continue;
            }
            let mut raw = queued.raw;
            if let Some(filter) = self.state.lock().filter.clone() {
                let keep = filter.filter(&mut raw)?;
                if keep > raw.len() {
                    return Err(AxError::InvalidInput);
                }
                raw.truncate(keep);
            }
            let Some(pkt) = DccpDispatcher::packet(&raw) else {
                continue;
            };
            let mut s = self.state.lock();
            if s.local != Some(pkt.local) || !Self::accepts_coverage(&s, &pkt) {
                continue;
            }
            s.gsr = pkt.seq;
            Self::take_features(&mut s, &pkt.options);
            let request_acked = matches!(s.phase, Phase::Request)
                && pkt
                    .ack
                    .is_some_and(|a| s.out.iter().any(|x| x.typ == REQUEST && x.seq == a));
            let response_acked = matches!(s.phase, Phase::Respond)
                && pkt
                    .ack
                    .is_some_and(|a| s.out.iter().any(|x| x.typ == RESPONSE && x.seq == a));
            if let Some(a) = pkt.ack {
                s.out.retain(|x| !seq_acked(x.seq, a));
                s.flight = s.out.iter().filter(|x| x.typ == DATA).count() as u32;
                if s.out.is_empty() && s.phase != Phase::TimeWait {
                    s.deadline = 0;
                }
            }
            match (pkt.typ, s.phase) {
                (RESPONSE, Phase::Request) if request_acked => {
                    if pkt.service != Some(s.service) {
                        continue;
                    }
                    s.phase = Phase::Open;
                    s.peer = Some(pkt.peer);
                    drop(s);
                    self.transmit(ACK, &[], None, false)?;
                    self.poll.wake();
                }
                (REQUEST, Phase::Listen) => {
                    if s.embryos.len() + s.pending.len() < s.backlog {
                        let service = pkt.service.unwrap_or(0);
                        let snapshot = self.passive_snapshot(&s);
                        // A passive child borrows the listener's already
                        // published port reservation.  Its mutable socket
                        // option snapshot is intentionally separate from the
                        // reservation-time SO_REUSEADDR decision.
                        let reservation_reuse = s
                            .bound_reuse_address
                            .expect("a DCCP listener owns a local-port reservation");
                        drop(s);
                        let child = Self::new(self.stack.clone(), self.family)?;
                        child.apply_passive_snapshot(snapshot)?;
                        {
                            let mut c = child.state.lock();
                            c.local = Some(pkt.local);
                            c.peer = Some(pkt.peer);
                            c.service = service;
                            c.gsr = pkt.seq;
                            c.phase = Phase::Respond;
                        }
                        let dont_route = self.general.dont_route();
                        child
                            .general
                            .set_option_inner(SetSocketOption::DontRoute(&dont_route))?;
                        self.dispatcher.install(
                            &child.ingress,
                            pkt.local,
                            Some(pkt.peer),
                            pkt.local,
                            false,
                            reservation_reuse,
                            Some(self.ingress.id),
                        )?;
                        if let Err(error) = child.transmit(RESPONSE, &[], None, true) {
                            self.dispatcher.remove(&child.ingress);
                            return Err(error);
                        }
                        self.state.lock().embryos.push_back(child);
                        self.poll.wake();
                    }
                }
                (ACK, Phase::Respond) if response_acked => {
                    s.phase = Phase::Open;
                    self.poll.wake();
                }
                (DATA | DATAACK, Phase::Open) => {
                    let p = pkt.peer;
                    let data = pkt.payload;
                    drop(s);
                    self.transmit(ACK, &[], None, false)?;
                    let mut s = self.state.lock();
                    s.rx.push_back((p, data));
                    self.poll.wake();
                }
                (CLOSEREQ | CLOSE, _) => {
                    s.phase = Phase::TimeWait;
                    // A terminal peer close cannot inherit an earlier
                    // request retransmission.  TIMEWAIT owns its deadline
                    // and never enters the RTO path.
                    s.out.clear();
                    s.deadline = tnow + s.server_timewait;
                    drop(s);
                    let _ = self.transmit(CLOSE, &[], None, false);
                    self.rd.store(true, Ordering::Release);
                    self.wr.store(true, Ordering::Release);
                    self.poll.wake();
                }
                (RESET, _) => {
                    s.last_error = pkt.reset.unwrap_or(0);
                    s.phase = Phase::Closed;
                    s.deadline = 0;
                    s.out.clear();
                    self.set_pending_error(if pkt.reset == Some(7) {
                        SocketFault::ConnectionRefused
                    } else {
                        SocketFault::ConnectionReset
                    });
                    self.rd.store(true, Ordering::Release);
                    self.wr.store(true, Ordering::Release);
                    self.poll.wake();
                }
                (SYNC, Phase::Open) => {
                    drop(s);
                    self.transmit(SYNCACK, &[], None, false)?
                }
                _ => {}
            }
        }
        self.promote_embryos();
        self.flush_data()?;
        Ok(())
    }
    /// A listener owns only completed children.  The dispatcher routes the
    /// final handshake ACK to each embryo; promotion is deliberately done
    /// only after that child has consumed the ACK.
    fn promote_embryos(&self) {
        let mut s = self.state.lock();
        let mut i = 0;
        while i < s.embryos.len() {
            let _ = s.embryos[i].drive();
            let phase = s.embryos[i].state.lock().phase;
            if phase == Phase::Open {
                let child = s.embryos.remove(i).unwrap();
                s.pending.push_back(child);
                self.poll.wake();
            } else if phase == Phase::Closed {
                let child = s.embryos.remove(i).unwrap();
                self.dispatcher.remove(&child.ingress);
            } else {
                i += 1;
            }
        }
    }
    pub fn prepare_accept(&self) -> AxResult<DccpAcceptReservation<'_>> {
        self.drive()?;
        self.promote_embryos();
        let s = self.state.lock();
        let c = s.pending.front().ok_or(AxError::WouldBlock)?;
        Ok(DccpAcceptReservation {
            listener: self,
            peer: c.state.lock().peer.unwrap(),
            token: c.accept,
        })
    }
    pub fn disconnect(&self) -> AxResult {
        let _lifecycle = self.lifecycle.lock();
        let mut s = self.state.lock();
        let local = s.bound_local;
        let reservation_reuse = s.bound_reuse_address;
        let was_listener = s.phase == Phase::Listen;
        let unaccepted_child_ids: Vec<_> = s
            .pending
            .iter()
            .chain(s.embryos.iter())
            .map(|child| child.ingress.id)
            .collect();
        // Keep a bound reservation installed while atomically replacing the
        // connected route.  Removing it first lets another endpoint steal the
        // port before the unconnected route is published.
        if let Some(local) = local {
            let reservation_reuse =
                reservation_reuse.expect("a DCCP local-port reservation records SO_REUSEADDR");
            if was_listener {
                // The child routes borrow this listener's reservation.  They
                // must leave the routing table in the same transaction as the
                // listener replacement; ordinary conflict admission would see
                // them first and reject the listener's own disconnect.
                self.dispatcher.replace_listener_with_bound(
                    &self.ingress,
                    local,
                    reservation_reuse,
                    &unaccepted_child_ids,
                )?;
            } else {
                self.dispatcher.install(
                    &self.ingress,
                    local,
                    None,
                    local,
                    false,
                    reservation_reuse,
                    None,
                )?;
            }
        } else {
            self.dispatcher.remove(&self.ingress);
            self.ingress.replace_route();
        }
        // Draining the queues in separate steps avoids overlapping mutable
        // borrows through two VecDeque::drain iterators.
        let pending = core::mem::take(&mut s.pending);
        let embryos = core::mem::take(&mut s.embryos);
        let children: Vec<_> = pending.into_iter().chain(embryos).collect();
        s.peer = None;
        s.local = local;
        s.phase = Phase::Idle;
        s.rx.clear();
        s.tx.clear();
        s.out.clear();
        s.flight = 0;
        s.deadline = 0;
        s.last_error = 0;
        s.gsr = 0;
        s.iss = NEXT.fetch_add(1, Ordering::Relaxed) & SEQ_MASK;
        s.seq = s.iss;
        s.peer_ccid = 2;
        s.short_seq = false;
        self.rd.store(false, Ordering::Release);
        self.wr.store(false, Ordering::Release);
        drop(s);
        for child in children {
            self.dispatcher.remove(&child.ingress);
        }
        self.general.clear_pending_error();
        if local.is_some() {
            self.general.set_device_mask(u64::MAX);
        } else {
            self.general.set_device_mask(0);
        }
        self.poll.wake();
        Ok(())
    }
    /// Return a failed active open to a bound-but-unconnected socket.  Both
    /// explicit binds and the implicit autobind performed by connect(2) keep
    /// their local port, matching the post-failure Linux socket identity and
    /// making a later reconnect use the same reservation.  The ingress epoch
    /// makes this a no-op if a concurrent disconnect/reconnect has already
    /// published a newer route.
    fn rollback_failed_connect_locked(&self, connect_epoch: u64, peer: SocketAddr) -> AxResult {
        let mut state = self.state.lock();
        if self.ingress.epoch.load(Ordering::Acquire) != connect_epoch
            || !matches!(
                state.phase,
                Phase::Request | Phase::Closed | Phase::TimeWait
            )
            || state.peer != Some(peer)
        {
            return Ok(());
        }
        let Some(local) = state.bound_local else {
            self.dispatcher.remove(&self.ingress);
            self.ingress.replace_route();
            state.local = None;
            state.peer = None;
            state.phase = Phase::Idle;
            self.general.set_device_mask(0);
            return Ok(());
        };
        let reservation_reuse = state
            .bound_reuse_address
            .expect("a DCCP local-port reservation records SO_REUSEADDR");
        if let Err(error) = self.dispatcher.install(
            &self.ingress,
            local,
            None,
            local,
            false,
            reservation_reuse,
            None,
        ) {
            // Do not publish an unconnected state while a connected route is
            // still live.  If preserving the reservation itself failed (for
            // example allocation failure), remove both and make the socket
            // unbound rather than leaking a stale peer tuple.
            self.dispatcher.remove(&self.ingress);
            self.ingress.replace_route();
            state.bound_local = None;
            state.bound_reuse_address = None;
            state.local = None;
            state.peer = None;
            state.phase = Phase::Idle;
            state.rx.clear();
            state.tx.clear();
            state.out.clear();
            state.flight = 0;
            state.deadline = 0;
            self.rd.store(false, Ordering::Release);
            self.wr.store(false, Ordering::Release);
            self.general.set_device_mask(0);
            self.poll.wake();
            return Err(error);
        }
        state.local = Some(local);
        state.peer = None;
        state.phase = Phase::Idle;
        state.rx.clear();
        state.tx.clear();
        state.out.clear();
        state.flight = 0;
        state.deadline = 0;
        state.last_error = 0;
        state.gsr = 0;
        state.iss = NEXT.fetch_add(1, Ordering::Relaxed) & SEQ_MASK;
        state.seq = state.iss;
        state.peer_ccid = 2;
        state.short_seq = false;
        self.rd.store(false, Ordering::Release);
        self.wr.store(false, Ordering::Release);
        self.general.set_device_mask(u64::MAX);
        self.poll.wake();
        Ok(())
    }
    fn rollback_failed_connect(&self, connect_epoch: u64, peer: SocketAddr) -> AxResult {
        let _lifecycle = self.lifecycle.lock();
        self.rollback_failed_connect_locked(connect_epoch, peer)
    }
}
impl Drop for DccpSocket {
    fn drop(&mut self) {
        self.dispatcher.remove(&self.ingress)
    }
}
impl Configurable for DccpSocket {
    fn nonblocking(&self) -> bool {
        self.general.nonblocking()
    }
    fn get_option_inner(&self, o: &mut GetSocketOption) -> AxResult<bool> {
        self.general.get_option_inner(o)
    }
    fn set_option_inner(&self, o: SetSocketOption) -> AxResult<bool> {
        self.general.set_option_inner(o)
    }
}
impl SocketOps for DccpSocket {
    fn bind(&self, a: SocketAddrEx) -> AxResult {
        let _lifecycle = self.lifecycle.lock();
        let mut a = a.into_ip().map_err(|_| AxError::InvalidInput)?;
        if !matches!(
            (self.family, a.ip()),
            (RawSocketFamily::Ipv4, IpAddr::V4(_)) | (RawSocketFamily::Ipv6, IpAddr::V6(_))
        ) {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.state.lock();
        if state.phase != Phase::Idle || state.local.is_some() {
            return Err(AxError::ResourceBusy);
        }
        // Follow TCP's bind validation exactly.  The service accepts the
        // wildcard address and rejects a concrete address which is not local
        // to this network namespace.
        self.stack.get_service().validate_bind_addr(a.ip().into())?;
        let reservation_reuse = self.general.reuse_address();
        if a.port() == 0 {
            a = self.dispatcher.install_ephemeral(
                &self.ingress,
                a.ip(),
                None,
                false,
                reservation_reuse,
            )?;
        } else {
            self.dispatcher
                .install(&self.ingress, a, None, a, false, reservation_reuse, None)?;
        }
        state.bound_local = Some(a);
        state.bound_reuse_address = Some(reservation_reuse);
        state.local = Some(a);
        self.general.set_device_mask(u64::MAX);
        Ok(())
    }
    fn connect(&self, a: SocketAddrEx) -> AxResult {
        let p = a.into_ip().map_err(|_| AxError::InvalidInput)?;
        if !matches!(
            (self.family, p.ip()),
            (RawSocketFamily::Ipv4, IpAddr::V4(_)) | (RawSocketFamily::Ipv6, IpAddr::V6(_))
        ) {
            return Err(AxError::InvalidInput);
        }
        let lifecycle = self.lifecycle.lock();
        let (connect_epoch, device_mask) = {
            let mut s = self.state.lock();
            match s.phase {
                Phase::Request => return Err(AxError::InProgress),
                Phase::Idle => {}
                _ => return Err(AxError::AlreadyConnected),
            }
            // A wildcard bind reserves the port but is not a valid on-wire
            // source for a connected DCCP handshake.  Select the routed
            // source just as TCP does, preserving the already-reserved port;
            // publish it only after dispatcher installation succeeds.
            let previous_bound = s.bound_local;
            let reservation_reuse = previous_bound
                .map(|_| {
                    s.bound_reuse_address
                        .expect("a DCCP local-port reservation records SO_REUSEADDR")
                })
                .unwrap_or_else(|| self.general.reuse_address());
            let (source, device_mask) = self.stack.resolve_raw_outbound(
                p.ip(),
                previous_bound
                    .filter(|local| !local.ip().is_unspecified())
                    .map(|local| local.ip()),
                self.general.dont_route(),
            )?;
            let l = if let Some(local) = previous_bound {
                let local = SocketAddr::new(source, local.port());
                self.dispatcher.install(
                    &self.ingress,
                    local,
                    Some(p),
                    previous_bound.unwrap(),
                    false,
                    reservation_reuse,
                    None,
                )?;
                local
            } else {
                self.dispatcher.install_ephemeral(
                    &self.ingress,
                    source,
                    Some(p),
                    false,
                    reservation_reuse,
                )?
            };
            // An implicit bind is a real reservation, not merely a route
            // selected for this REQUEST.  Keep it in state so disconnect and
            // reconnect retain the local port until the socket is closed.
            if previous_bound.is_none() {
                s.bound_local = Some(l);
                s.bound_reuse_address = Some(reservation_reuse);
            }
            s.local = Some(l);
            s.peer = Some(p);
            s.phase = Phase::Request;
            (self.ingress.epoch.load(Ordering::Acquire), device_mask)
        };
        self.general.set_device_mask(device_mask);
        if let Err(error) = self.transmit(REQUEST, &[], None, true) {
            if let Err(rollback_error) = self.rollback_failed_connect_locked(connect_epoch, p) {
                return Err(rollback_error);
            }
            return Err(error);
        }
        drop(lifecycle);
        let result = self.general.connect_poller(self, || {
            self.drive()?;
            let s = self.state.lock();
            match s.phase {
                Phase::Open => Ok(()),
                Phase::Closed => match s.last_error {
                    u8::MAX => Err(AxError::TimedOut),
                    7 => Err(AxError::ConnectionRefused),
                    _ => Err(AxError::ConnectionReset),
                },
                Phase::Request | Phase::Respond => Err(AxError::WouldBlock),
                _ => Err(AxError::NotConnected),
            }
        });
        match result {
            Err(error) if error != AxError::WouldBlock => {
                if let Err(rollback_error) = self.rollback_failed_connect(connect_epoch, p) {
                    Err(rollback_error)
                } else {
                    Err(error)
                }
            }
            result => result,
        }
    }
    fn listen(&self, n: usize) -> AxResult {
        let _lifecycle = self.lifecycle.lock();
        let mut s = self.state.lock();
        if s.phase == Phase::Listen {
            s.backlog = cmp::min(n.max(1), MAX);
            self.poll.wake();
            return Ok(());
        }
        if s.phase != Phase::Idle {
            return Err(AxError::InvalidInput);
        }
        let (l, reservation_reuse) = if let Some(local) = s.local {
            let reservation_reuse = s
                .bound_reuse_address
                .expect("a DCCP local-port reservation records SO_REUSEADDR");
            self.dispatcher.install(
                &self.ingress,
                local,
                None,
                local,
                true,
                reservation_reuse,
                None,
            )?;
            (local, reservation_reuse)
        } else {
            let unspecified = match self.family {
                RawSocketFamily::Ipv4 => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                RawSocketFamily::Ipv6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            };
            let reservation_reuse = self.general.reuse_address();
            let local = self.dispatcher.install_ephemeral(
                &self.ingress,
                unspecified,
                None,
                true,
                reservation_reuse,
            )?;
            (local, reservation_reuse)
        };
        s.bound_local = Some(l);
        s.bound_reuse_address = Some(reservation_reuse);
        s.local = Some(l);
        s.phase = Phase::Listen;
        s.backlog = cmp::min(n.max(1), MAX);
        Ok(())
    }
    fn accept(&self) -> AxResult<Socket> {
        self.prepare_accept()?.commit()
    }
    fn send(&self, r: impl Read + IoBuf, _: SendOptions) -> AxResult<usize> {
        self.send_data(r, 0)
    }
    fn recv(&self, mut w: impl Write + IoBufMut, o: RecvOptions<'_>) -> AxResult<usize> {
        self.drive()?;
        let mut s = self.state.lock();
        // A packet queued before a peer close stays readable.  This also
        // keeps a deferred fault one-shot: payload is delivered first and the
        // fault is observed by the terminal transfer poller afterwards.
        let Some((p, b)) = s.rx.front().cloned() else {
            return if self.rd.load(Ordering::Acquire) {
                Ok(0)
            } else {
                Err(AxError::WouldBlock)
            };
        };
        if let Some(f) = o.from {
            *f = SocketAddrEx::Ip(p)
        }
        let n = w.write(&b)?;
        if !o.flags.contains(RecvFlags::PEEK) {
            s.rx.pop_front();
        }
        Ok(if o.flags.contains(RecvFlags::TRUNCATE) {
            b.len()
        } else {
            n
        })
    }
    fn local_addr(&self) -> AxResult<SocketAddrEx> {
        Ok(SocketAddrEx::Ip(
            self.state.lock().local.ok_or(AxError::NotFound)?,
        ))
    }
    fn peer_addr(&self) -> AxResult<SocketAddrEx> {
        Ok(SocketAddrEx::Ip(
            self.state.lock().peer.ok_or(AxError::NotConnected)?,
        ))
    }
    fn shutdown(&self, h: Shutdown) -> AxResult {
        let _lifecycle = self.lifecycle.lock();
        if h.has_read() {
            self.rd.store(true, Ordering::Release)
        }
        if h.has_write() {
            let phase = self.state.lock().phase;
            if phase == Phase::Open {
                self.transmit(CLOSE, &[], None, true)?;
                self.state.lock().phase = Phase::Closing
            }
            self.wr.store(true, Ordering::Release)
        }
        self.poll.wake();
        Ok(())
    }
}
impl Pollable for DccpSocket {
    fn poll(&self) -> IoEvents {
        let _ = self.drive();
        let s = self.state.lock();
        let mut e = IoEvents::empty();
        e.set(
            IoEvents::READABLE,
            self.rd.load(Ordering::Acquire)
                || !s.rx.is_empty()
                || (s.phase == Phase::Listen && !s.pending.is_empty()),
        );
        e.set(
            IoEvents::WRITABLE,
            !self.wr.load(Ordering::Acquire) && s.phase == Phase::Open,
        );
        e.set(
            IoEvents::READ_HANGUP,
            self.rd.load(Ordering::Acquire) || s.phase == Phase::Closed,
        );
        self.general.add_pending_error_event(e)
    }
    fn register<'a>(
        &'a self,
        c: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        self.arm_timer(c)?;
        let mut p = PreparedPollRegistration::try_new(6)?;
        p.arm(&self.poll, c.waker())?;
        p.arm(&self.ingress.wake, c.waker())?;
        self.dispatcher.v4.arm_dispatcher_readiness(&mut p, c)?;
        self.dispatcher.v6.arm_dispatcher_readiness(&mut p, c)?;
        p.commit()
    }
}
