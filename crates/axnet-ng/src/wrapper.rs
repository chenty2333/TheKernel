use alloc::vec;

use axerrno::{AxError, AxResult};
use axsync::Mutex;
use event_listener::Event;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::{AnySocket, Socket},
    wire::IpAddress,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Transport {
    Tcp,
    Udp,
}

fn addrs_conflict(requested: Option<IpAddress>, existing: Option<IpAddress>) -> bool {
    match (requested, existing) {
        (None, _) | (_, None) => true,
        (Some(requested), Some(existing)) => requested == existing,
    }
}

pub(crate) struct SocketSetWrapper<'a> {
    pub inner: Mutex<SocketSet<'a>>,
    pub new_socket: Event,
}

impl<'a> SocketSetWrapper<'a> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SocketSet::new(vec![])),
            new_socket: Event::new(),
        }
    }

    pub fn add<T: AnySocket<'a>>(&self, socket: T) -> SocketHandle {
        let handle = self.inner.lock().add(socket);
        debug!("socket {}: created", handle);
        self.new_socket.notify(1);
        handle
    }

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

    pub fn remove(&self, handle: SocketHandle) {
        self.inner.lock().remove(handle);
        debug!("socket {}: destroyed", handle);
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
