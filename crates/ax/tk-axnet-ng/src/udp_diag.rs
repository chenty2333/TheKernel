//! Read-only UDP endpoint and occupied payload-ring observations.
use super::*;

#[derive(Clone, Copy, Debug)]
pub struct UdpDiagnosticSnapshot {
    pub local: IpEndpoint,
    pub peer: Option<IpEndpoint>,
    /// Native occupied ring bytes, including wrap padding, not Linux skb truesize.
    pub receive_queue: usize,
    pub send_queue: usize,
}

impl UdpSocket {
    pub fn diagnostic_snapshot(&self, nowait: bool) -> AxResult<Option<UdpDiagnosticSnapshot>> {
        // send holds cork before autobind's transition mutex and socket-set.
        // Preserve that order; do not use with_smol_socket (takes pending errors).
        let cork = if nowait {
            self.cork.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            self.cork.lock()
        };
        let _transition = if nowait {
            self.transition.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            self.transition.lock()
        };
        let local = if nowait {
            self.local_state
                .try_read()
                .ok_or(AxError::WouldBlock)?
                .endpoint
        } else {
            self.local_state.read().endpoint
        };
        let route = if nowait {
            *self.route_state.try_read().ok_or(AxError::WouldBlock)?
        } else {
            *self.route_state.read()
        };
        let Some(local) = route.reported_local_addr.or(local) else {
            return Ok(None);
        };
        let sockets = if nowait {
            self.stack
                .socket_set
                .inner
                .try_lock()
                .ok_or(AxError::WouldBlock)?
        } else {
            self.stack.socket_set.inner.lock()
        };
        let socket = sockets.get::<smol::Socket>(self.handle);
        Ok(Some(UdpDiagnosticSnapshot {
            local,
            peer: route.peer_addr.map(|(peer, _)| peer),
            receive_queue: socket.recv_queue(),
            send_queue: socket
                .send_queue()
                .saturating_add(cork.as_ref().map_or(0, |pending| pending.payload.len())),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_selects_real_bind_connect_and_cork_without_consuming() {
        let stack = NetStack::new_loopback_only();
        let socket = UdpSocket::new(stack.clone()).unwrap();
        assert!(socket.diagnostic_snapshot(false).unwrap().is_none());
        let address = |port| {
            SocketAddrEx::Ip(core::net::SocketAddr::new(
                core::net::Ipv4Addr::LOCALHOST.into(),
                port,
            ))
        };
        socket.bind(address(32620)).unwrap();
        let first = socket.diagnostic_snapshot(false).unwrap().unwrap();
        assert_eq!(first.local.port, 32620);
        assert_eq!(first.peer, None);
        socket.connect(address(32621)).unwrap();
        socket
            .send(
                &b"pending"[..],
                SendOptions {
                    flags: SendFlags::MORE,
                    ..SendOptions::default()
                },
            )
            .unwrap();
        for _ in 0..2 {
            let connected = socket.diagnostic_snapshot(false).unwrap().unwrap();
            assert_eq!(connected.peer.unwrap().port, 32621);
            assert_eq!(connected.send_queue, 7);
        }
        let _held = stack.socket_set.inner.lock();
        assert!(matches!(
            socket.diagnostic_snapshot(true),
            Err(AxError::WouldBlock)
        ));
    }

    #[test]
    fn nowait_does_not_wait_on_publication_or_cork() {
        let socket = UdpSocket::new(NetStack::new_loopback_only()).unwrap();
        {
            let _held = socket.cork.lock();
            assert!(matches!(
                socket.diagnostic_snapshot(true),
                Err(AxError::WouldBlock)
            ));
        }
        let _held = socket.transition.lock();
        assert!(matches!(
            socket.diagnostic_snapshot(true),
            Err(AxError::WouldBlock)
        ));
    }
}
