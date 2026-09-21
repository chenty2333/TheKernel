//! `file::netlink` subsections; see the parent `mod.rs` for the module map.

use super::*;

static SOCK_DIAG_NEXT_COOKIE: AtomicU64 = AtomicU64::new(1);

/// Lifetime-owned registry entry for an inet socket.  The registration is
/// retained by the socket OFD, so close/fork/exec naturally share and retire
/// the same diagnostic identity without a process-global socket table.
pub(crate) struct SocketDiagRegistration {
pub(crate)     net_ns: Weak<NetworkNamespace>,
pub(crate)     family: u16,
    socket_type: u8,
    protocol: u8,
pub(crate)     cookie: u64,
}

impl SocketDiagRegistration {
pub(crate) fn diag_state(&self) -> u8 {
        // TCP and DCCP inet_diag use the normal close state for an OFD which
        // has no transport-state snapshot yet. Datagram diagnostics carry no
        // state bit rather than pretending to be a TCP endpoint.
        if self.socket_type == SOCK_STREAM as u8
            || self.socket_type == SOCK_SEQPACKET as u8
            || self.protocol == 33
        {
            7
        } else {
            0
        }
    }
}

pub(crate) fn register_socket_diag(
    net_ns: &Arc<NetworkNamespace>,
    family: u16,
    socket_type: u8,
    protocol: u8,
) -> AxResult<Arc<SocketDiagRegistration>> {
    let registration = Arc::try_new(SocketDiagRegistration {
        net_ns: Arc::downgrade(net_ns),
        family,
        socket_type,
        protocol,
        cookie: SOCK_DIAG_NEXT_COOKIE.fetch_add(1, Ordering::Relaxed),
    })
    .map_err(|_| AxError::NoMemory)?;
    let mut registrations = SOCK_DIAG_REGISTRATIONS.lock();
    registrations.retain(|entry| entry.strong_count() != 0);
    registrations
        .try_reserve(1)
        .map_err(|_| AxError::NoMemory)?;
    registrations.push(Arc::downgrade(&registration));
    Ok(registration)
}

#[derive(Clone, Copy)]
pub(crate) struct InetDiagRequest {
    family: u8,
    protocol: u8,
pub(crate)     extensions: u8,
    states: u32,
    sport: u16,
    dport: u16,
    src: [u8; 16],
    dst: [u8; 16],
    ifindex: u32,
    cookie: [u32; 2],
}

impl InetDiagRequest {
pub(crate) fn parse(payload: &[u8]) -> AxResult<Self> {
        debug_assert_eq!(payload.len(), INET_DIAG_REQ_V2_LEN);
        let states = u32::from_ne_bytes(payload[4..8].try_into().unwrap());
        let sport = u16::from_be_bytes(payload[8..10].try_into().unwrap());
        let dport = u16::from_be_bytes(payload[10..12].try_into().unwrap());
        let mut src = [0_u8; 16];
        let mut dst = [0_u8; 16];
        src.copy_from_slice(&payload[12..28]);
        dst.copy_from_slice(&payload[28..44]);
        Ok(Self {
            family: payload[0],
            protocol: payload[1],
            extensions: payload[2],
            states,
            sport,
            dport,
            src,
            dst,
            ifindex: u32::from_ne_bytes(payload[44..48].try_into().unwrap()),
            cookie: [
                u32::from_ne_bytes(payload[48..52].try_into().unwrap()),
                u32::from_ne_bytes(payload[52..56].try_into().unwrap()),
            ],
        })
    }

pub(crate) fn matches(&self, entry: &SocketDiagRegistration) -> bool {
        if (self.family != AF_UNSPEC as u8 && self.family as u16 != entry.family)
            || (self.protocol != 0 && self.protocol != entry.protocol)
        {
            return false;
        }
        let state = entry.diag_state();
        if self.states != 0 && (state == 0 || self.states & (1_u32 << (state - 1)) == 0) {
            return false;
        }
        // Registered transport endpoints currently retain the canonical
        // unbound sockid. Therefore any nonzero address/port/interface filter
        // cannot match; cookie still identifies the exact live OFD.
        if self.sport != 0
            || self.dport != 0
            || self.src.iter().any(|&v| v != 0)
            || self.dst.iter().any(|&v| v != 0)
            || self.ifindex != 0
        {
            return false;
        }
        self.cookie == [INET_DIAG_NOCOOKIE; 2]
            || self.cookie == [entry.cookie as u32, (entry.cookie >> 32) as u32]
    }
}
