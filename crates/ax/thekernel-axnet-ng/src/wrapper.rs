use alloc::vec::Vec;

use axerrno::{AxError, AxResult};
use axsync::Mutex;
use event_listener::Event;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::{AnySocket, Socket},
    wire::IpAddress,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Transport {
    Tcp,
    Udp,
}

/// Fixed active-socket bound for one network namespace.  The owned storage is
/// preallocated to this size, and [`SocketSetWrapper::add`] rejects a full
/// set before smoltcp can grow it.  Keeping the storage owned avoids leaking a
/// per-namespace `SocketStorage` array merely to manufacture a `'static`
/// borrow.
pub(crate) const MAX_SOCKETS: usize = 128;

fn addrs_conflict(requested: Option<IpAddress>, existing: Option<IpAddress>) -> bool {
    match (requested, existing) {
        (None, _) | (_, None) => true,
        (Some(requested), Some(existing)) => requested == existing,
    }
}

struct ClosingTcp {
    handle: SocketHandle,
    queued: usize,
    timeout: Duration,
    deadline: Instant,
}

pub(crate) struct SocketSetWrapper<'a> {
    pub inner: Mutex<SocketSet<'a>>,
    pub new_socket: Event,
    // Lock order: inner, then closing_tcp. Entries remain in the existing
    // bounded socket set until protocol close (including TIME-WAIT) finishes.
    closing_tcp: Mutex<Vec<ClosingTcp>>,
}

impl<'a> SocketSetWrapper<'a> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SocketSet::new(Vec::with_capacity(MAX_SOCKETS))),
            new_socket: Event::new(),
            closing_tcp: Mutex::new(Vec::with_capacity(MAX_SOCKETS)),
        }
    }

    pub fn add<T: AnySocket<'a>>(&self, socket: T) -> AxResult<SocketHandle> {
        let mut sockets = self.inner.lock();
        if sockets.iter().count() >= MAX_SOCKETS {
            return Err(AxError::ResourceBusy);
        }
        let handle = sockets.add(socket);
        debug!("socket {handle}: created");
        self.new_socket.notify(1);
        Ok(handle)
    }

    #[cfg(test)]
    pub fn with_socket<T: AnySocket<'a>, R, F>(&self, handle: SocketHandle, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        let set = self.inner.lock();
        let socket = set.get(handle);
        f(socket)
    }

    pub fn with_socket_mut<T: AnySocket<'a>, R, F>(&self, handle: SocketHandle, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        let mut set = self.inner.lock();
        let socket = set.get_mut(handle);
        f(socket)
    }

    /// Nonblocking counterpart for per-operation NOWAIT paths.  Socket-set
    /// ownership is normally invisible to callers, but treating this mutex
    /// as an exception would make MSG_DONTWAIT sleep behind another socket.
    pub fn try_with_socket_mut<T: AnySocket<'a>, R, F>(
        &self,
        handle: SocketHandle,
        f: F,
    ) -> AxResult<R>
    where
        F: FnOnce(&mut T) -> R,
    {
        let mut set = self.inner.try_lock().ok_or(AxError::WouldBlock)?;
        Ok(f(set.get_mut(handle)))
    }

    fn local_endpoint(
        socket: &Socket<'_>,
        transport: Transport,
    ) -> Option<(Option<IpAddress>, u16)> {
        match (transport, socket) {
            (Transport::Tcp, Socket::Tcp(s)) => {
                Some((s.get_bound_endpoint().addr, s.get_bound_endpoint().port))
            }
            (Transport::Udp, Socket::Udp(s)) => Some((s.endpoint().addr, s.endpoint().port)),
            _ => None,
        }
    }

    pub fn bind_check(&self, transport: Transport, addr: Option<IpAddress>, port: u16) -> AxResult {
        if port == 0 {
            return Ok(());
        }

        let mut sockets = self.inner.lock();
        for (_, socket) in sockets.iter_mut() {
            let Some((existing_addr, existing_port)) = Self::local_endpoint(socket, transport)
            else {
                continue;
            };
            if existing_port == port && addrs_conflict(addr, existing_addr) {
                return Err(AxError::AddrInUse);
            }
        }
        Ok(())
    }

    pub fn port_in_use(&self, transport: Transport, port: u16) -> bool {
        if port == 0 {
            return false;
        }

        let mut sockets = self.inner.lock();
        sockets.iter_mut().any(|(_, socket)| {
            Self::local_endpoint(socket, transport)
                .is_some_and(|(_, existing_port)| existing_port == port)
        })
    }

    /// Release the file-description owner without discarding accepted TX bytes.
    /// The service's existing TCP timer/packet polling owns the remaining close.
    pub fn close_tcp(&self, handle: SocketHandle, now: Instant) {
        let mut sockets = self.inner.lock();
        let socket = sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle);
        socket.close();
        if socket.state() == smoltcp::socket::tcp::State::Closed && socket.remote_endpoint().is_none() {
            sockets.remove(handle);
        } else {
            // Preserve an explicitly configured transport timeout. The orphan
            // deadline tracks ACKed bytes rather than arbitrary incoming ACKs:
            // zero-window probes must not retain a dead fd's buffer forever.
            let timeout = socket.timeout().unwrap_or(Duration::from_secs(60));
            let entry = ClosingTcp {
                handle,
                queued: socket.send_queue(),
                timeout,
                deadline: now + timeout,
            };
            let mut closing = self.closing_tcp.lock();
            debug_assert!(closing.len() < MAX_SOCKETS);
            closing.push(entry);
        }
    }

    pub fn closing_tcp_deadline(&self) -> Option<Instant> {
        self.closing_tcp.lock().iter().map(|entry| entry.deadline).min()
    }

    /// Called while the service owns `inner`, after processing TCP timers and
    /// packets. Returns whether an abort needs another egress pass for its RST.
    pub fn reap_closed_tcp(&self, sockets: &mut SocketSet<'_>, now: Instant) -> bool {
        let mut aborted = false;
        self.closing_tcp.lock().retain_mut(|entry| {
            let socket = sockets.get_mut::<smoltcp::socket::tcp::Socket>(entry.handle);
            if socket.state() == smoltcp::socket::tcp::State::Closed {
                // This runs after the next egress opportunity following abort.
                // RST is best effort: a down link/full route must not retain
                // an expired owner forever or rearm an already expired timer.
                sockets.remove(entry.handle);
                return false;
            }
            let queued = socket.send_queue();
            if queued < entry.queued {
                entry.queued = queued;
                entry.deadline = now + entry.timeout;
            } else if now >= entry.deadline {
                socket.abort();
                aborted = true;
            }
            true
        });
        aborted
    }

    pub fn remove(&self, handle: SocketHandle) {
        self.inner.lock().remove(handle);
        debug!("socket {handle}: destroyed");
    }
}

#[cfg(test)]
mod tests {
    use smoltcp::wire::{IpAddress, Ipv4Address};

    use super::addrs_conflict;

    #[test]
    fn wildcard_bind_conflicts_with_specific_addr() {
        assert!(addrs_conflict(
            None,
            Some(IpAddress::Ipv4(Ipv4Address::LOCALHOST))
        ));
        assert!(addrs_conflict(
            Some(IpAddress::Ipv4(Ipv4Address::LOCALHOST)),
            None
        ));
    }

    #[test]
    fn specific_bind_only_conflicts_with_same_addr() {
        assert!(addrs_conflict(
            Some(IpAddress::Ipv4(Ipv4Address::LOCALHOST)),
            Some(IpAddress::Ipv4(Ipv4Address::LOCALHOST))
        ));
        assert!(!addrs_conflict(
            Some(IpAddress::Ipv4(Ipv4Address::LOCALHOST)),
            Some(IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 2)))
        ));
    }
}
