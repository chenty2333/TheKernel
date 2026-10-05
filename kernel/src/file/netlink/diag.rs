//! `file::netlink` subsections; see the parent `mod.rs` for the module map.

use super::*;

static SOCK_DIAG_NEXT_COOKIE: AtomicU64 = AtomicU64::new(1);

/// The socket retains this token; the registry and token retain only weak
/// references back to the endpoint, so close cannot form a reference cycle.
pub(crate) struct SocketDiagRegistration {
    pub(crate) net_ns: Weak<NetworkNamespace>,
    pub(crate) family: u16,
    protocol: u8,
    pub(crate) cookie: u64,
    owner: Mutex<Option<Weak<dyn FileLike>>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SocketDiagRecord {
    pub(crate) family: u16,
    protocol: u8,
    pub(crate) state: u8,
    pub(crate) cookie: u64,
    pub(crate) sport: u16,
    pub(crate) dport: u16,
    pub(crate) src: [u8; 16],
    pub(crate) dst: [u8; 16],
    pub(crate) ifindex: u32,
    pub(crate) receive_queue: u32,
    pub(crate) send_queue: u32,
    pub(crate) uid: u32,
    pub(crate) inode: u32,
}

fn diagnostic_address(address: Option<IpAddress>, family: u16) -> [u8; 16] {
    let mut out = [0; 16];
    match address {
        Some(IpAddress::Ipv4(address)) if family == AF_INET6 as u16 => {
            out[10..12].copy_from_slice(&[0xff, 0xff]);
            out[12..16].copy_from_slice(&address.octets());
        }
        Some(IpAddress::Ipv4(address)) => out[..4].copy_from_slice(&address.octets()),
        Some(IpAddress::Ipv6(address)) => out.copy_from_slice(&address.octets()),
        None => {}
    }
    out
}

impl SocketDiagRegistration {
    pub(crate) fn attach_owner(&self, owner: Weak<dyn FileLike>) {
        *self.owner.lock() = Some(owner);
    }

    fn snapshot(&self, nowait: bool) -> AxResult<Option<SocketDiagRecord>> {
        let owner = if nowait {
            self.owner.try_lock().ok_or(AxError::WouldBlock)?.clone()
        } else {
            self.owner.lock().clone()
        };
        let Some(owner) = owner.and_then(|owner| owner.upgrade()) else {
            return Ok(None);
        };
        let Some(socket) = owner.downcast_ref::<crate::file::Socket>() else {
            return Ok(None);
        };
        // No canonical CLOSED/unbound rows for transports without a provider.
        let axnet::Socket::Tcp(tcp) = &socket.inner else {
            return Ok(None);
        };
        let snapshot = match tcp.diagnostic_snapshot(nowait) {
            Ok(snapshot) => snapshot,
            Err(AxError::WouldBlock) if !nowait => return Ok(None),
            Err(error) => return Err(error),
        };
        // Linux TCP dumps do not enumerate anonymous, unbound TCP_CLOSE OFDs.
        if snapshot.local.port == 0 {
            return Ok(None);
        }
        let (inode, uid) = socket.diag_inode_owner();
        Ok(Some(SocketDiagRecord {
            family: self.family,
            protocol: self.protocol,
            state: snapshot.state,
            cookie: self.cookie,
            sport: snapshot.local.port,
            dport: snapshot.peer.map_or(0, |peer| peer.port),
            src: diagnostic_address(snapshot.local.addr, self.family),
            dst: diagnostic_address(snapshot.peer.map(|peer| peer.addr), self.family),
            ifindex: socket.bound_device_index() as u32,
            receive_queue: snapshot.receive_queue as u32,
            send_queue: snapshot.send_queue as u32,
            uid,
            inode: inode as u32,
        }))
    }
}

/// Release the registry before taking endpoint/transport locks. Capture all
/// observations during write admission, before committing frames,
/// so NOWAIT failure cannot enqueue a partial dump from this datagram.
pub(crate) fn diagnostic_records(
    net_ns: &Arc<NetworkNamespace>,
    nowait: bool,
    actor: &Cred,
) -> AxResult<Vec<SocketDiagRecord>> {
    let uid_map = if nowait {
        actor.user_ns().try_uid_map()?
    } else {
        actor.user_ns().uid_map()
    };
    let namespace = Arc::downgrade(net_ns);
    let entries = {
        let mut registry = if nowait {
            SOCK_DIAG_REGISTRATIONS
                .try_lock()
                .ok_or(AxError::WouldBlock)?
        } else {
            SOCK_DIAG_REGISTRATIONS.lock()
        };
        registry.retain(|entry| entry.strong_count() != 0);
        let mut entries = Vec::new();
        for entry in registry.iter().filter_map(Weak::upgrade) {
            if Weak::ptr_eq(&entry.net_ns, &namespace) {
                entries.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                entries.push(entry);
            }
        }
        entries
    };
    let mut records = Vec::new();
    records
        .try_reserve(entries.len())
        .map_err(|_| AxError::NoMemory)?;
    for entry in entries {
        if let Some(mut record) = entry.snapshot(nowait)? {
            record.uid = crate::task::Kuid::from_raw(record.uid)
                .and_then(|uid| uid_map.kernel_uid_to_user(uid))
                .map_or(65534, |uid| uid.into_raw());
            records.push(record);
        }
    }
    Ok(records)
}

pub(crate) fn register_socket_diag(
    net_ns: &Arc<NetworkNamespace>,
    family: u16,
    protocol: u8,
) -> AxResult<Arc<SocketDiagRegistration>> {
    let registration = Arc::try_new(SocketDiagRegistration {
        net_ns: Arc::downgrade(net_ns),
        family,
        protocol,
        cookie: SOCK_DIAG_NEXT_COOKIE.fetch_add(1, Ordering::Relaxed),
        owner: Mutex::new(None),
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
    pub(crate) extensions: u8,
    states: u32,
    sport: u16,
    dport: u16,
}

impl InetDiagRequest {
    pub(crate) fn parse(payload: &[u8]) -> AxResult<Self> {
        debug_assert_eq!(payload.len(), INET_DIAG_REQ_V2_LEN);
        let states = u32::from_ne_bytes(payload[4..8].try_into().unwrap());
        let sport = u16::from_be_bytes(payload[8..10].try_into().unwrap());
        let dport = u16::from_be_bytes(payload[10..12].try_into().unwrap());
        Ok(Self {
            family: payload[0],
            protocol: payload[1],
            extensions: payload[2],
            states,
            sport,
            dport,
        })
    }

    pub(crate) fn matches(&self, entry: &SocketDiagRecord) -> bool {
        if (self.family != AF_UNSPEC as u8 && self.family as u16 != entry.family)
            || (self.protocol != 0 && self.protocol != entry.protocol)
        {
            return false;
        }
        // Bound TCP_CLOSE sockets are selected by TCPF_BOUND_INACTIVE, while
        // their emitted base-record state remains TCP_CLOSE (Linux 7.2.3).
        let state = if entry.state == 7 { 13 } else { entry.state };
        if self.states & (1_u32 << state) == 0 {
            return false;
        }
        // In dump mode Linux uses only the port selectors here. Address,
        // interface and cookie selection belongs to bytecode/exact lookup;
        // real ss submits a zero cookie, not INET_DIAG_NOCOOKIE.
        entry.state == 7
            || ((self.sport == 0 || self.sport == entry.sport)
                && (self.dport == 0 || self.dport == entry.dport))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> SocketDiagRecord {
        SocketDiagRecord {
            family: AF_INET as u16,
            protocol: 6,
            state: 10,
            cookie: 0x123456789abcdef0,
            sport: 24680,
            dport: 0,
            src: diagnostic_address(Some(Ipv4Address::LOCALHOST.into()), AF_INET as u16),
            dst: [0; 16],
            ifindex: 2,
            receive_queue: 3,
            send_queue: 5,
            uid: 1000,
            inode: 42,
        }
    }

    #[test]
    fn state_masks_use_state_numbers_and_zero_selects_nothing() {
        let mut bytes = [0; INET_DIAG_REQ_V2_LEN];
        bytes[0] = AF_INET as u8;
        bytes[1] = 6;
        bytes[48..56].fill(0xff);
        let mut request = InetDiagRequest::parse(&bytes).unwrap();
        let mut entry = record();
        assert!(!request.matches(&entry));
        request.states = 1 << 9;
        assert!(!request.matches(&entry));
        request.states = 1 << 10;
        assert!(request.matches(&entry));
        request.sport = entry.sport;
        assert!(request.matches(&entry));
        request.sport += 1;
        assert!(!request.matches(&entry));
        request.sport = 0;
        entry.state = 7;
        request.states = 1 << 7;
        assert!(!request.matches(&entry));
        request.states = 1 << 13;
        assert!(request.matches(&entry));
        // Dump sockid cookie/address/interface fields are not exact selectors.
        bytes[4..8].copy_from_slice(&(1u32 << 13).to_ne_bytes());
        bytes[12..56].fill(0);
        assert!(InetDiagRequest::parse(&bytes).unwrap().matches(&entry));
        bytes[12..56].fill(0x5a);
        assert!(InetDiagRequest::parse(&bytes).unwrap().matches(&entry));
    }

    #[test]
    fn emitted_identity_endpoints_and_queues_use_inet_diag_layout() {
        let entry = record();
        let header = NlMsgHdr {
            nlmsg_len: 0,
            nlmsg_type: SOCK_DIAG_BY_FAMILY,
            nlmsg_flags: 0,
            nlmsg_seq: 42,
            nlmsg_pid: 0,
        };
        let bytes = sock_diag_message(&header, 123, &entry, 0);
        let payload = &bytes[size_of::<NlMsgHdr>()..];
        assert_eq!(payload.len(), 72);
        assert_eq!(&payload[..2], &[AF_INET as u8, 10]);
        assert_eq!(&payload[4..6], &24680u16.to_be_bytes());
        assert_eq!(&payload[8..12], &[127, 0, 0, 1]);
        for (offset, expected) in [(40, 2u32), (56, 3), (60, 5), (64, 1000), (68, 42)] {
            assert_eq!(&payload[offset..offset + 4], &expected.to_ne_bytes());
        }
        assert_eq!(&payload[44..52], &entry.cookie.to_ne_bytes());
        let mapped = diagnostic_address(Some(Ipv4Address::LOCALHOST.into()), AF_INET6 as u16);
        assert_eq!(&mapped[10..], &[255, 255, 127, 0, 0, 1]);
    }
}
