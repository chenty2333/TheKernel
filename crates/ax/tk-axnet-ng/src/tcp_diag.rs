//! Read-only TCP observations. No polling, acknowledgement, or queue consumption.

use super::*;

#[derive(Clone, Copy, Debug)]
pub struct TcpDiagnosticSnapshot {
    /// Linux TCP state number, not the wrapper's connection transition state.
    pub state: u8,
    pub local: IpListenEndpoint,
    pub peer: Option<IpEndpoint>,
    /// Bytes for connected sockets; completed accept entries for listeners.
    pub receive_queue: usize,
    /// Bytes for connected sockets; admitted backlog capacity for listeners.
    pub send_queue: usize,
}

fn linux_state(state: smol::State) -> u8 {
    match state {
        smol::State::Established => 1,
        smol::State::SynSent => 2,
        smol::State::SynReceived => 3,
        smol::State::FinWait1 => 4,
        smol::State::FinWait2 => 5,
        smol::State::TimeWait => 6,
        smol::State::Closed => 7,
        smol::State::CloseWait => 8,
        smol::State::LastAck => 9,
        smol::State::Listen => 10,
        smol::State::Closing => 11,
    }
}

impl TcpSocket {
    /// Snapshot under the existing socket-set -> listener-entry lock order.
    /// NOWAIT callers never wait for either mutex or a wrapper transition.
    pub fn diagnostic_snapshot(&self, nowait: bool) -> AxResult<TcpDiagnosticSnapshot> {
        let sockets = if nowait {
            self.stack
                .socket_set
                .inner
                .try_lock()
                .ok_or(AxError::WouldBlock)?
        } else {
            self.stack.socket_set.inner.lock()
        };
        let state = self.state();
        // Busy is a bind/listen/connect publication in progress, not TCP_CLOSE.
        if state == State::Busy {
            return Err(AxError::WouldBlock);
        }
        let socket = sockets.get::<smol::Socket>(self.handle);
        // Accepted children and wildcard binds have a concrete connection
        // tuple, independent of the wrapper's bind-admission metadata.
        let local = socket.local_endpoint().map_or_else(
            || socket.get_bound_endpoint(),
            |endpoint| IpListenEndpoint {
                addr: Some(endpoint.addr),
                port: endpoint.port,
            },
        );
        let (state, receive_queue, send_queue) = if state == State::Listening {
            let (ready, limit) = self
                .stack
                .listen_table
                .diagnostic_backlog(local.port, &sockets, nowait)?;
            (10, ready, limit)
        } else {
            (
                linux_state(socket.state()),
                socket.recv_queue(),
                socket.send_queue(),
            )
        };
        Ok(TcpDiagnosticSnapshot {
            state,
            local,
            peer: socket.remote_endpoint(),
            receive_queue,
            send_queue,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_report_binding_backlog_and_never_wait_on_socket_set() {
        let stack = NetStack::new_loopback_only();
        let socket = TcpSocket::new(stack.clone()).unwrap();
        let empty = socket.diagnostic_snapshot(false).unwrap();
        assert_eq!(
            (
                empty.state,
                empty.local.port,
                empty.receive_queue,
                empty.send_queue
            ),
            (7, 0, 0, 0)
        );
        socket
            .bind(SocketAddrEx::Ip(SocketAddr::new(
                Ipv4Addr::LOCALHOST.into(),
                32510,
            )))
            .unwrap();
        socket.listen(3).unwrap();
        let listener = socket.diagnostic_snapshot(false).unwrap();
        assert_eq!(
            (
                listener.state,
                listener.local.port,
                listener.receive_queue,
                listener.send_queue
            ),
            (10, 32510, 0, 3)
        );
        assert_eq!(
            listener.local.addr,
            Some(smoltcp::wire::Ipv4Address::LOCALHOST.into())
        );
        let _held = stack.socket_set.inner.lock();
        assert!(matches!(
            socket.diagnostic_snapshot(true),
            Err(AxError::WouldBlock)
        ));
    }

    #[test]
    fn all_transport_states_have_linux_numbers() {
        let states = [
            smol::State::Established,
            smol::State::SynSent,
            smol::State::SynReceived,
            smol::State::FinWait1,
            smol::State::FinWait2,
            smol::State::TimeWait,
            smol::State::Closed,
            smol::State::CloseWait,
            smol::State::LastAck,
            smol::State::Listen,
            smol::State::Closing,
        ];
        for (index, state) in states.into_iter().enumerate() {
            assert_eq!(linux_state(state), index as u8 + 1);
        }
    }
}
