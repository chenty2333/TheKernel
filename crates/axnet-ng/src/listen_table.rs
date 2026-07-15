use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use core::{
    ops::DerefMut,
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axsync::Mutex;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::tcp::{self, State},
    wire::{IpEndpoint, IpListenEndpoint},
};

use crate::{consts::LISTEN_QUEUE_SIZE, tcp::new_tcp_socket, wrapper::SocketSetWrapper};

const PORT_NUM: usize = 65536;

struct ListenTableEntryInner {
    generation: u64,
    listen_endpoint: IpListenEndpoint,
    syn_queue: VecDeque<SocketHandle>,
    accept_reservations: usize,
    queue_limit: usize,
    socket_set: alloc::sync::Weak<SocketSetWrapper<'static>>,
}

impl ListenTableEntryInner {
    pub fn new(
        generation: u64,
        listen_endpoint: IpListenEndpoint,
        backlog: usize,
        socket_set: alloc::sync::Weak<SocketSetWrapper<'static>>,
    ) -> Self {
        let queue_limit = backlog.clamp(1, LISTEN_QUEUE_SIZE);
        Self {
            generation,
            listen_endpoint,
            syn_queue: VecDeque::with_capacity(queue_limit),
            accept_reservations: 0,
            queue_limit,
            socket_set,
        }
    }
}

impl Drop for ListenTableEntryInner {
    fn drop(&mut self) {
        if let Some(ss) = self.socket_set.upgrade() {
            for &handle in &self.syn_queue {
                ss.remove(handle);
            }
        }
    }
}

pub struct ListenTable {
    tcp: Box<[Mutex<Option<Box<ListenTableEntryInner>>>]>,
    next_generation: AtomicU64,
}

impl ListenTable {
    pub fn new() -> Self {
        Self::try_new().expect("failed to allocate TCP listen table")
    }

    /// Fallibly creates an empty bounded TCP listen table.
    pub fn try_new() -> AxResult<Self> {
        let tcp = unsafe {
            let mut buf = Box::try_new_uninit_slice(PORT_NUM).map_err(|_| AxError::NoMemory)?;
            for i in 0..PORT_NUM {
                buf[i].write(Mutex::new(None));
            }
            buf.assume_init()
        };
        Ok(Self {
            tcp,
            next_generation: AtomicU64::new(1),
        })
    }

    pub(crate) fn listen(
        &self,
        listen_endpoint: IpListenEndpoint,
        backlog: usize,
        socket_set: &Arc<SocketSetWrapper<'static>>,
    ) -> AxResult {
        let port = listen_endpoint.port;
        assert_ne!(port, 0);
        let mut entry = self.tcp[port as usize].lock();
        if entry.is_none() {
            let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
            *entry = Some(Box::new(ListenTableEntryInner::new(
                generation,
                listen_endpoint,
                backlog,
                Arc::downgrade(socket_set),
            )));
            Ok(())
        } else {
            warn!("socket already listening on port {port}");
            Err(AxError::AddrInUse)
        }
    }

    pub(crate) fn set_backlog(&self, port: u16, backlog: usize) -> AxResult {
        let entry = self.listen_entry(port);
        let mut entry = entry.lock();
        let entry = entry.as_mut().ok_or(AxError::InvalidInput)?;
        entry.queue_limit = backlog.clamp(1, LISTEN_QUEUE_SIZE);
        Ok(())
    }

    pub fn unlisten(&self, port: u16) {
        debug!("TCP socket unlisten on {port}");
        *self.tcp[port as usize].lock() = None;
    }

    fn listen_entry(&self, port: u16) -> &Mutex<Option<Box<ListenTableEntryInner>>> {
        &self.tcp[port as usize]
    }

    pub(crate) fn can_accept(&self, port: u16, socket_set: &SocketSetWrapper) -> AxResult<bool> {
        if let Some(entry) = self.listen_entry(port).lock().as_ref() {
            Ok(entry
                .syn_queue
                .iter()
                .any(|&handle| is_connected(handle, socket_set)))
        } else {
            warn!("accept before listen");
            Err(AxError::InvalidInput)
        }
    }

    pub(crate) fn reserve_accept(
        &self,
        port: u16,
        socket_set: &SocketSetWrapper,
    ) -> AxResult<(SocketHandle, u64)> {
        let entry = self.listen_entry(port);
        let mut table = entry.lock();
        let Some(entry) = table.deref_mut() else {
            warn!("accept before listen");
            return Err(AxError::InvalidInput);
        };

        let syn_queue: &mut VecDeque<SocketHandle> = &mut entry.syn_queue;
        let idx = syn_queue
            .iter()
            .enumerate()
            .find_map(|(idx, &handle)| is_connected(handle, socket_set).then_some(idx))
            .ok_or(AxError::WouldBlock)?; // wait for connection
        if idx > 0 {
            warn!(
                "slow SYN queue enumeration: index = {}, len = {}!",
                idx,
                syn_queue.len()
            );
        }
        let handle = syn_queue.swap_remove_front(idx).unwrap();
        entry.accept_reservations += 1;
        // If the connection is reset, return ConnectionReset error
        // Otherwise, return the handle and the address tuple
        if is_closed(handle, socket_set) {
            warn!("accept failed: connection reset");
            entry.accept_reservations -= 1;
            socket_set.remove(handle);
            Err(AxError::ConnectionReset)
        } else {
            Ok((handle, entry.generation))
        }
    }

    pub(crate) fn commit_accept(&self, port: u16, generation: u64) -> bool {
        let mut table = self.listen_entry(port).lock();
        let Some(entry) = table.as_deref_mut() else {
            return false;
        };
        if entry.generation != generation || entry.accept_reservations == 0 {
            return false;
        }
        entry.accept_reservations -= 1;
        true
    }

    pub(crate) fn restore_accept(&self, port: u16, generation: u64, handle: SocketHandle) -> bool {
        let mut table = self.listen_entry(port).lock();
        let Some(entry) = table.as_deref_mut() else {
            return false;
        };
        if entry.generation != generation || entry.accept_reservations == 0 {
            return false;
        }
        entry.accept_reservations -= 1;
        entry.syn_queue.push_front(handle);
        true
    }

    pub fn incoming_tcp_packet(
        &self,
        src: IpEndpoint,
        dst: IpEndpoint,
        sockets: &mut SocketSet<'_>,
    ) {
        if let Some(entry) = self.listen_entry(dst.port).lock().deref_mut() {
            if entry
                .listen_endpoint
                .addr
                .is_some_and(|addr| addr != dst.addr)
            {
                return;
            }
            if entry.syn_queue.len() + entry.accept_reservations >= entry.queue_limit {
                // SYN queue is full, drop the packet
                warn!("SYN queue overflow!");
                return;
            }

            let Ok(mut socket) = new_tcp_socket() else {
                warn!("Failed to allocate TCP buffers for an incoming connection");
                return;
            };
            if let Err(err) = socket.listen(entry.listen_endpoint) {
                warn!("Failed to listen on {}: {:?}", entry.listen_endpoint, err);
                return;
            }
            let handle = sockets.add(socket);
            debug!(
                "TCP socket {}: prepare for connection {} -> {}",
                handle, src, entry.listen_endpoint
            );
            entry.syn_queue.push_back(handle);
        }
    }
}

fn is_connected(handle: SocketHandle, socket_set: &SocketSetWrapper) -> bool {
    socket_set.with_socket::<tcp::Socket, _, _>(handle, |socket| {
        !matches!(socket.state(), State::Listen | State::SynReceived)
    })
}

fn is_closed(handle: SocketHandle, socket_set: &SocketSetWrapper) -> bool {
    socket_set
        .with_socket::<tcp::Socket, _, _>(handle, |socket| matches!(socket.state(), State::Closed))
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use smoltcp::wire::Ipv4Address;

    use super::*;

    #[test]
    fn listen_backlog_is_nonzero_and_bounded() {
        let endpoint = IpListenEndpoint {
            addr: Some(Ipv4Address::LOCALHOST.into()),
            port: 1234,
        };
        let empty = alloc::sync::Weak::<SocketSetWrapper<'static>>::new();

        assert_eq!(
            ListenTableEntryInner::new(1, endpoint, 0, empty.clone()).queue_limit,
            1
        );
        assert_eq!(
            ListenTableEntryInner::new(2, endpoint, usize::MAX, empty).queue_limit,
            LISTEN_QUEUE_SIZE
        );
    }

    #[test]
    fn accept_restore_is_bound_to_one_listener_generation() {
        let table = ListenTable::try_new().unwrap();
        let socket_set = Arc::new(SocketSetWrapper::new());
        let endpoint = IpListenEndpoint {
            addr: Some(Ipv4Address::LOCALHOST.into()),
            port: 2345,
        };
        table.listen(endpoint, 2, &socket_set).unwrap();
        let handle = socket_set.add(new_tcp_socket().unwrap());
        let generation = {
            let mut table_entry = table.listen_entry(endpoint.port).lock();
            let entry = table_entry.as_deref_mut().unwrap();
            entry.accept_reservations = 1;
            entry.generation
        };

        assert!(table.restore_accept(endpoint.port, generation, handle));
        {
            let table_entry = table.listen_entry(endpoint.port).lock();
            let entry = table_entry.as_deref().unwrap();
            assert_eq!(entry.accept_reservations, 0);
            assert_eq!(entry.syn_queue.front(), Some(&handle));
        }

        let handle = {
            let mut table_entry = table.listen_entry(endpoint.port).lock();
            let entry = table_entry.as_deref_mut().unwrap();
            entry.accept_reservations = 1;
            entry.syn_queue.pop_front().unwrap()
        };
        table.unlisten(endpoint.port);
        table.listen(endpoint, 2, &socket_set).unwrap();
        assert!(!table.restore_accept(endpoint.port, generation, handle));
        socket_set.remove(handle);
    }
}
