use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec};
use core::ops::DerefMut;

use axerrno::{AxError, AxResult};
use axsync::Mutex;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::tcp::{self, SocketBuffer, State},
    wire::{IpEndpoint, IpListenEndpoint},
};

use crate::{
    consts::{LISTEN_QUEUE_SIZE, TCP_RX_BUF_LEN, TCP_TX_BUF_LEN},
    wrapper::SocketSetWrapper,
};

const PORT_NUM: usize = 65536;

struct ListenTableEntryInner {
    listen_endpoint: IpListenEndpoint,
    syn_queue: VecDeque<SocketHandle>,
    socket_set: alloc::sync::Weak<SocketSetWrapper<'static>>,
}

impl ListenTableEntryInner {
    pub fn new(
        listen_endpoint: IpListenEndpoint,
        socket_set: alloc::sync::Weak<SocketSetWrapper<'static>>,
    ) -> Self {
        Self {
            listen_endpoint,
            syn_queue: VecDeque::with_capacity(LISTEN_QUEUE_SIZE),
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

type ListenTableEntry = Arc<Mutex<Option<Box<ListenTableEntryInner>>>>;

pub struct ListenTable {
    tcp: Box<[ListenTableEntry]>,
}

impl ListenTable {
    pub fn new() -> Self {
        let tcp = unsafe {
            let mut buf = Box::new_uninit_slice(PORT_NUM);
            for i in 0..PORT_NUM {
                buf[i].write(Arc::default());
            }
            buf.assume_init()
        };
        Self { tcp }
    }

    pub fn can_listen(&self, port: u16) -> bool {
        self.tcp[port as usize].lock().is_none()
    }

    pub(crate) fn listen(
        &self,
        listen_endpoint: IpListenEndpoint,
        socket_set: &Arc<SocketSetWrapper<'static>>,
    ) -> AxResult {
        let port = listen_endpoint.port;
        assert_ne!(port, 0);
        let mut entry = self.tcp[port as usize].lock();
        if entry.is_none() {
            *entry = Some(Box::new(ListenTableEntryInner::new(
                listen_endpoint,
                Arc::downgrade(socket_set),
            )));
            Ok(())
        } else {
            warn!("socket already listening on port {port}");
            Err(AxError::AddrInUse)
        }
    }

    pub fn unlisten(&self, port: u16) {
        debug!("TCP socket unlisten on {}", port);
        *self.tcp[port as usize].lock() = None;
    }

    fn listen_entry(&self, port: u16) -> Arc<Mutex<Option<Box<ListenTableEntryInner>>>> {
        self.tcp[port as usize].clone()
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

    pub(crate) fn accept(
        &self,
        port: u16,
        socket_set: &SocketSetWrapper,
    ) -> AxResult<SocketHandle> {
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
        // If the connection is reset, return ConnectionReset error
        // Otherwise, return the handle and the address tuple
        if is_closed(handle, socket_set) {
            warn!("accept failed: connection reset");
            Err(AxError::ConnectionReset)
        } else {
            Ok(handle)
        }
    }

    pub fn incoming_tcp_packet(
        &self,
        src: IpEndpoint,
        dst: IpEndpoint,
        sockets: &mut SocketSet<'_>,
    ) {
        if let Some(entry) = self.listen_entry(dst.port).lock().deref_mut() {
            // TODO(mivik): accept address check
            if entry.syn_queue.len() >= LISTEN_QUEUE_SIZE {
                // SYN queue is full, drop the packet
                warn!("SYN queue overflow!");
                return;
            }

            let mut socket = smoltcp::socket::tcp::Socket::new(
                SocketBuffer::new(vec![0; TCP_RX_BUF_LEN]),
                SocketBuffer::new(vec![0; TCP_TX_BUF_LEN]),
            );
            if let Err(err) = socket.listen(IpListenEndpoint {
                addr: None,
                port: dst.port,
            }) {
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
