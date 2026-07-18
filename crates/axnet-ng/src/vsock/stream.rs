use alloc::sync::Arc;
use core::task::Context;

use axerrno::{AxError, AxResult, LinuxError, ax_bail, ax_err_type};
use axio::prelude::*;
use axpoll::{
    IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable, PreparedPollRegistration,
};
use axsync::Mutex;

use super::connection_manager::*;
use crate::{
    RecvFlags, RecvOptions, SendOptions, Shutdown,
    device::*,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, SetSocketOption},
    state::*,
    vsock::{VsockAddr, VsockConnId, VsockTransport, VsockTransportOps},
};

/// Stream transport for vsock sockets.
pub struct VsockStreamTransport {
    conn_id: Mutex<Option<VsockConnId>>,
    connection: Mutex<Option<Arc<Mutex<Connection>>>>,
    state: StateLock,
    general: GeneralOptions,
    poll_state: PollSet,
}

/// Exact vsock connection retained outside its listener queue until accept
/// policy either commits or restores it.
pub struct VsockAcceptReservation {
    prepared: Option<PreparedVsockAccept>,
}

impl VsockAcceptReservation {
    pub fn connection_identity(&self) -> VsockConnId {
        self.prepared
            .as_ref()
            .expect("active vsock accept reservation")
            .conn_id
    }

    pub fn commit(mut self) -> AxResult<(VsockTransport, VsockAddr)> {
        let prepared = self
            .prepared
            .take()
            .expect("active vsock accept reservation");
        let next_accept = {
            let mut manager = VSOCK_CONN_MANAGER.lock();
            manager.commit_accept(&prepared)?
        };
        if let Some(accept_poll) = next_accept {
            accept_poll.wake();
        }
        let peer_addr = prepared.peer_addr;
        let new_transport = VsockStreamTransport {
            conn_id: Mutex::new(Some(prepared.conn_id)),
            connection: Mutex::new(Some(prepared.connection)),
            state: StateLock::new(State::Connected),
            general: GeneralOptions::default(),
            poll_state: PollSet::new(),
        };
        Ok((VsockTransport::Stream(new_transport), peer_addr))
    }
}

impl Drop for VsockAcceptReservation {
    fn drop(&mut self) {
        let Some(prepared) = self.prepared.take() else {
            return;
        };
        let accept_poll = {
            let mut manager = VSOCK_CONN_MANAGER.lock();
            manager.restore_accept(&prepared)
        };
        if let Some(accept_poll) = accept_poll {
            accept_poll.wake();
        }
    }
}

impl VsockStreamTransport {
    /// Create a new idle vsock stream transport.
    pub fn new() -> Self {
        Self {
            conn_id: Mutex::new(None),
            connection: Mutex::new(None),
            state: StateLock::new(State::Idle),
            general: GeneralOptions::new(),
            poll_state: PollSet::new(),
        }
    }

    pub(super) fn retry_transfer<T>(
        &self,
        direction: crate::SocketTransferDirection,
        effective_nonblocking: bool,
        attempt: &mut impl FnMut() -> AxResult<T>,
    ) -> AxResult<T> {
        self.general
            .transfer_poller(self, direction, effective_nonblocking, attempt)
    }

    fn get_connection(&self) -> AxResult<Arc<Mutex<Connection>>> {
        self.connection.lock().clone().ok_or(AxError::NotConnected)
    }

    pub fn prepare_accept(&self) -> AxResult<VsockAcceptReservation> {
        if self.state.get() != State::Listening {
            ax_bail!(InvalidInput, "not listening");
        }

        let connection = self.get_connection()?;
        let local_port = connection.lock().local_addr().port;
        self.general.recv_poller(self, || {
            let prepared = VSOCK_CONN_MANAGER.lock().prepare_accept(local_port)?;
            Ok(VsockAcceptReservation {
                prepared: Some(prepared),
            })
        })
    }
}

impl Default for VsockStreamTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Configurable for VsockStreamTransport {
    fn nonblocking(&self) -> bool {
        self.general.nonblocking()
    }

    fn get_option_inner(&self, opt: &mut GetSocketOption) -> AxResult<bool> {
        self.general.get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> AxResult<bool> {
        self.general.set_option_inner(opt)
    }
}

impl VsockTransportOps for VsockStreamTransport {
    fn set_pending_error(&self, error: LinuxError) {
        self.general.set_pending_error(error);
    }

    fn bind(&self, mut local_addr: VsockAddr) -> AxResult<()> {
        self.state
            .lock(State::Idle)
            .map_err(|_| ax_err_type!(InvalidInput, "already bound"))?
            .transit(State::Idle, || {
                let mut manager = VSOCK_CONN_MANAGER.lock();
                if local_addr.port == 0 {
                    local_addr.port = manager.allocate_port()?;
                }
                let conn_id = VsockConnId::listening(local_addr.port);
                let conn =
                    manager.create_connection(conn_id, local_addr, None, ConnectionState::Idle)?;

                *self.conn_id.lock() = Some(conn_id);
                *self.connection.lock() = Some(conn);
                trace!("Vsock binding to {local_addr:?}");
                Ok(())
            })?;
        self.poll_state.wake();
        Ok(())
    }

    fn listen(&self, backlog: usize) -> AxResult<()> {
        let guard = self
            .state
            .lock(State::Idle)
            .map_err(|_| ax_err_type!(InvalidInput, "invalid state for listen"))?;

        guard.transit(State::Listening, || {
            let conn = self.get_connection()?;
            let local_addr = conn.lock().local_addr();

            // register in the global listen table
            VSOCK_CONN_MANAGER.lock().listen(local_addr, backlog)?;
            vsock_listen(local_addr)?;
            // set state
            conn.lock().set_state(ConnectionState::Listening);
            trace!("Vsock listening on {local_addr:?}");
            Ok(())
        })?;
        self.poll_state.wake();
        Ok(())
    }

    fn accept(&self) -> AxResult<(VsockTransport, VsockAddr)> {
        self.prepare_accept()?.commit()
    }

    fn connect(&self, peer_addr: VsockAddr) -> AxResult<()> {
        let guard = self.state.lock(State::Idle).map_err(|state| match state {
            State::Idle => unreachable!(),
            State::Listening => ax_err_type!(InvalidInput, "already listening"),
            State::Connecting => ax_err_type!(InProgress),
            State::Connected => ax_err_type!(AlreadyConnected),
            _ => ax_err_type!(AlreadyConnected),
        })?;

        guard.transit(State::Connecting, || {
            // The event worker uses device -> manager -> connection ordering.
            // Snapshot the device-owned CID before acquiring the manager so
            // connect cannot form the inverse manager -> device edge.
            let guest_cid = vsock_guest_cid()?;
            let mut manager = VSOCK_CONN_MANAGER.lock();
            let existing_conn = self.connection.lock();

            // get local address
            let local_port = if let Some(conn) = existing_conn.as_ref() {
                let conn_guard = conn.lock();
                match conn_guard.state() {
                    ConnectionState::Idle => {
                        // already bound but not connected, reuse the port
                        conn_guard.local_addr().port
                    }
                    _ => {
                        // should not happen due to state check above
                        ax_bail!(InvalidInput, "already connected or listening");
                    }
                }
            } else {
                manager.allocate_port()?
            };
            drop(existing_conn);

            let local_addr = VsockAddr {
                cid: guest_cid,
                port: local_port,
            };

            // create connection
            let conn_id = VsockConnId {
                peer_addr,
                local_port,
            };
            let conn = manager.create_connection(
                conn_id,
                local_addr,
                Some(peer_addr),
                ConnectionState::Connecting,
            )?;

            *self.conn_id.lock() = Some(conn_id);
            *self.connection.lock() = Some(conn.clone());

            drop(manager);

            // driver connect
            vsock_connect(conn_id)?;
            debug!("Vsock connecting from {local_port} to {peer_addr:?}");
            Ok(())
        })?;

        self.poll_state.wake();

        // wait for connection established
        self.general.connect_poller(self, || {
            let conn = self.get_connection()?;
            let state = conn.lock().state();
            match state {
                ConnectionState::Connected => Ok(()),
                ConnectionState::Connecting => Err(AxError::WouldBlock),
                _ => Err(ax_err_type!(ConnectionRefused)),
            }
        })
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> AxResult<usize> {
        if !options.cmsg.is_empty() {
            return Err(AxError::OperationNotSupported);
        }
        self.general.consume_pending_error()?;
        let effective_nonblocking = options.effective_nonblocking(self.general.nonblocking());
        let conn_id = self.conn_id.lock().ok_or(AxError::NotConnected)?;
        let conn = self.get_connection()?;
        let conn_guard = conn.lock();

        if conn_guard.state() != ConnectionState::Connected {
            return Err(AxError::NotConnected);
        }

        if conn_guard.tx_closed() {
            return Err(AxError::NotConnected);
        }

        drop(conn_guard);

        // now virtio-driver only support non-blocking send
        if effective_nonblocking {
            // The current virtio-vsock transport is already nonblocking per
            // operation; retaining the flag here documents that no wait is
            // introduced below.
        }
        let result = src.write_to(&mut axio::write_fn(|buf| vsock_send(conn_id, buf)));
        conn.lock().add_tx_bytes(result.unwrap_or(0));
        result
    }

    fn recv(&self, mut dst: impl Write, options: RecvOptions) -> AxResult<usize> {
        let conn = self.get_connection()?;

        let effective_nonblocking = options.effective_nonblocking(self.general.nonblocking());
        self.general
            .recv_poller_with_effective_nonblocking(self, effective_nonblocking, || {
                let mut conn_guard = conn.lock();

                if conn_guard.rx_closed() && conn_guard.rx_buffer_used() == 0 {
                    return Ok(0); // EOF
                }

                // should allow read when connection is closed, to read remaining data
                if !matches!(
                    conn_guard.state(),
                    ConnectionState::Connected | ConnectionState::Closed
                ) {
                    return Err(AxError::NotConnected);
                }

                if conn_guard.rx_buffer_used() == 0 {
                    return Err(AxError::WouldBlock);
                }

                let (left, right) = conn_guard.rx_slices();
                let mut count = dst.write(left)?;

                if count >= left.len() && !right.is_empty() {
                    count += dst.write(right)?;
                }
                let released_capacity = if !options.flags.contains(RecvFlags::PEEK) {
                    conn_guard.advance_rx_read(count);
                    count != 0
                } else {
                    false
                };

                let buffer_remaining = conn_guard.rx_buffer_used();
                drop(conn_guard);
                if released_capacity {
                    // Capacity publication may interrupt the event worker. It
                    // must never run while the connection lock is held.
                    notify_vsock_rx_capacity();
                }

                if count > 0 {
                    trace!(
                        "Recv {} bytes from connection (buffer_remaining={}/{})",
                        count, buffer_remaining, VSOCK_RX_BUFFER_SIZE
                    );
                    Ok(count)
                } else {
                    Err(AxError::WouldBlock)
                }
            })
    }

    fn shutdown(&self, how: Shutdown) -> AxResult<()> {
        let conn_id = *self.conn_id.lock();
        let conn = self.get_connection()?;
        let (previous_state, local_port, rx_poll, connect_poll) = {
            let mut connection = conn.lock();

            if how.has_read() {
                connection.set_rx_closed(true);
            }

            if how.has_write() {
                connection.set_tx_closed(true);
            }

            let snapshot = (
                connection.state(),
                connection.local_addr().port,
                connection.rx_poll_source(),
                connection.connect_poll_source(),
            );
            connection.set_state(ConnectionState::Closed);
            snapshot
        };

        // External device/manager operations follow the connection commit;
        // none of them execute while the connection lock is held.
        let result = match (previous_state, conn_id) {
            (ConnectionState::Connected, Some(conn_id)) => vsock_disconnect(conn_id),
            (ConnectionState::Listening, _) => {
                VSOCK_CONN_MANAGER.lock().unlisten(local_port);
                Ok(())
            }
            _ => Ok(()),
        };
        rx_poll.wake();
        connect_poll.wake();
        self.poll_state.wake();
        result
    }

    fn local_addr(&self) -> AxResult<Option<VsockAddr>> {
        Ok(self
            .get_connection()
            .ok()
            .map(|conn| conn.lock().local_addr()))
    }

    fn peer_addr(&self) -> AxResult<Option<VsockAddr>> {
        Ok(self
            .get_connection()
            .ok()
            .and_then(|conn| conn.lock().peer_addr()))
    }
}

impl Pollable for VsockStreamTransport {
    fn poll(&self) -> IoEvents {
        let Ok(conn) = self.get_connection() else {
            return self.general.add_pending_error_event(IoEvents::empty());
        };

        let (state, local_port, rx_buffer_used, rx_closed, tx_closed) = {
            let conn = conn.lock();
            (
                conn.state(),
                conn.local_addr().port,
                conn.rx_buffer_used(),
                conn.rx_closed(),
                conn.tx_closed(),
            )
        };
        let mut events = IoEvents::empty();

        match state {
            ConnectionState::Listening => {
                // if there is a pending connection, set IN
                events.set(
                    IoEvents::READABLE,
                    VSOCK_CONN_MANAGER.lock().can_accept(local_port),
                );
            }
            ConnectionState::Connected | ConnectionState::Closed => {
                events.set(IoEvents::READABLE, rx_buffer_used > 0 || rx_closed);
                events.set(IoEvents::WRITABLE, !tx_closed);
            }
            ConnectionState::Connecting => {
                // Completion changes the connection state and wakes the
                // connect source; this snapshot remains non-writable.
                events.set(IoEvents::WRITABLE, false);
            }
            _ => {}
        }
        events.set(IoEvents::READ_HANGUP, rx_closed);
        self.general.add_pending_error_event(events)
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let dynamic_source = if let Ok(connection) = self.get_connection() {
            let connection = connection.lock();
            let state = connection.state();
            let local_port = connection.local_addr().port;
            let rx_source = connection.rx_poll_source();
            let connect_source = connection.connect_poll_source();
            drop(connection);

            match state {
                ConnectionState::Listening if events.contains(IoEvents::READABLE) => {
                    let queue = VSOCK_CONN_MANAGER
                        .lock()
                        .get_listen_queue(local_port)
                        .ok_or(PollRegistrationError::InvalidState)?;
                    Some(queue.lock().poll_source())
                }
                ConnectionState::Connected | ConnectionState::Closed
                    if events.intersects(
                        IoEvents::READABLE
                            | IoEvents::READ_HANGUP
                            | IoEvents::ERROR
                            | IoEvents::HANGUP,
                    ) =>
                {
                    Some(rx_source)
                }
                ConnectionState::Connecting if events.contains(IoEvents::WRITABLE) => {
                    Some(connect_source)
                }
                _ => None,
            }
        } else {
            None
        };

        let mut prepared =
            PreparedPollRegistration::try_new(1 + usize::from(dynamic_source.is_some()))?;
        if let Some(source) = dynamic_source {
            prepared.arm_owned(source, context.waker())?;
        }
        prepared.arm(&self.poll_state, context.waker())?;
        prepared.commit()
    }
}

impl Drop for VsockStreamTransport {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown(Shutdown::Both) {
            warn!("failed to shut down dropped vsock stream: {error:?}");
        }

        if let Some(conn_id) = *self.conn_id.lock() {
            VSOCK_CONN_MANAGER.lock().remove_connection(conn_id);
        }
    }
}
