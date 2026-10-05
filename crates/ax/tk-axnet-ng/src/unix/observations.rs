//! Read-only Unix OFD state; no polling, accept or receive queue consumption.
use super::*;

pub struct UnixDiagnosticSnapshot {
    pub socket_type: u16,
    pub connected: bool,
    pub listening: bool,
    pub address: UnixSocketAddr,
}

impl UnixSocket {
    pub fn diagnostic_snapshot(&self) -> UnixDiagnosticSnapshot {
        let (address, slot) = {
            let local = self.local.lock();
            (local.address.clone(), local.slot.clone())
        };
        // bind prepares queue storage, but only listen publishes a nonzero
        // connection-admission limit. Inspect that existing state, not the
        // mere presence of the preallocated receive queue.
        let connected = self.is_connected();
        let (socket_type, listening) = match &self.transport {
            Transport::Stream(_) => (
                1,
                !connected
                    && slot.as_ref().is_some_and(|slot| {
                        slot.stream
                            .lock()
                            .as_ref()
                            .is_some_and(|bind| bind.diagnostic_listening())
                    }),
            ),
            Transport::Dgram(_) => (2, false),
            Transport::SeqPacket(_) => (
                5,
                !connected
                    && slot.as_ref().is_some_and(|slot| {
                        slot.seqpacket
                            .lock()
                            .as_ref()
                            .is_some_and(|bind| bind.diagnostic_listening())
                    }),
            ),
        };
        UnixDiagnosticSnapshot {
            socket_type,
            connected,
            listening,
            address,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_queue_storage_is_not_listening_until_admission_is_published() {
        let namespace = UnixNamespace::try_new().unwrap();
        for transport in [
            Transport::Stream(StreamTransport::new().unwrap()),
            Transport::SeqPacket(SeqPacketTransport::new().unwrap()),
        ] {
            let socket = UnixSocket::new(transport, namespace.clone());
            let address = UnixSocketAddr::Abstract(Arc::new(Vec::from(&b"observed-listen"[..])));
            socket.bind(SocketAddrEx::Unix(address)).unwrap();
            assert!(!socket.diagnostic_snapshot().listening);
            socket.listen_as(0, SocketCredentials::default()).unwrap();
            assert!(socket.diagnostic_snapshot().listening);
            assert!(!socket.diagnostic_snapshot().connected);
            drop(socket);
            drain_endpoint_cleanup_work();
        }
    }

    #[test]
    fn anonymous_pairs_report_real_types_and_connected_state() {
        let namespace = UnixNamespace::try_new().unwrap();
        let credentials = SocketCredentials::default();
        let (stream, _) = StreamTransport::new_pair(credentials).unwrap();
        let (dgram, _) = DgramTransport::new_pair(credentials).unwrap();
        let (packet, _) = SeqPacketTransport::new_pair(credentials).unwrap();
        for (kind, socket) in [
            (1, UnixSocket::new(stream, namespace.clone())),
            (2, UnixSocket::new(dgram, namespace.clone())),
            (5, UnixSocket::new(packet, namespace)),
        ] {
            let snapshot = socket.diagnostic_snapshot();
            assert_eq!(snapshot.socket_type, kind);
            assert!(snapshot.connected);
            assert!(!snapshot.listening);
            assert!(matches!(snapshot.address, UnixSocketAddr::Unnamed));
        }
    }
}
