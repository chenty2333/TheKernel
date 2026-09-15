use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use axerrno::{AxError, AxResult};

use crate::consts::{SOCKET_BUFFER_MAX, SOCKET_BUFFER_MIN};

pub(crate) fn normalized_socket_buffer_size(requested: usize) -> usize {
    requested.clamp(SOCKET_BUFFER_MIN, SOCKET_BUFFER_MAX)
}

pub(crate) struct SocketBufferLimits {
    send: AtomicUsize,
    recv: AtomicUsize,
}

impl SocketBufferLimits {
    pub(crate) fn new(send: usize, recv: usize) -> Self {
        Self {
            send: AtomicUsize::new(normalized_socket_buffer_size(send)),
            recv: AtomicUsize::new(normalized_socket_buffer_size(recv)),
        }
    }

    pub(crate) fn send(&self) -> usize {
        self.send.load(Ordering::Acquire)
    }

    pub(crate) fn recv(&self) -> usize {
        self.recv.load(Ordering::Acquire)
    }

    pub(crate) fn set_send(&self, requested: usize) {
        self.send
            .store(normalized_socket_buffer_size(requested), Ordering::Release);
    }

    pub(crate) fn set_recv(&self, requested: usize) {
        self.recv
            .store(normalized_socket_buffer_size(requested), Ordering::Release);
    }
}

pub(crate) fn try_zeroed_socket_buffer(requested: usize) -> AxResult<Vec<u8>> {
    let size = normalized_socket_buffer_size(requested);
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(size)
        .map_err(|_| AxError::NoMemory)?;
    buffer.resize(size, 0);
    Ok(buffer)
}

pub(crate) fn try_filled_buffer<T: Clone>(len: usize, value: T) -> AxResult<Vec<T>> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| AxError::NoMemory)?;
    buffer.resize(len, value);
    Ok(buffer)
}

pub(crate) fn udp_packet_slots(payload_bytes: usize) -> usize {
    payload_bytes.div_ceil(2048).clamp(16, 512)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_buffer_sizes_are_bounded() {
        assert_eq!(normalized_socket_buffer_size(0), SOCKET_BUFFER_MIN);
        assert_eq!(normalized_socket_buffer_size(64 * 1024), 64 * 1024);
        assert_eq!(normalized_socket_buffer_size(usize::MAX), SOCKET_BUFFER_MAX);
    }

    #[test]
    fn udp_metadata_scales_with_payload_budget() {
        assert_eq!(udp_packet_slots(SOCKET_BUFFER_MIN), 16);
        assert_eq!(udp_packet_slots(256 * 1024), 128);
        assert_eq!(udp_packet_slots(SOCKET_BUFFER_MAX), 512);
    }

    #[test]
    fn socket_buffer_limits_store_normalized_capacities() {
        let limits = SocketBufferLimits::new(0, usize::MAX);
        assert_eq!(limits.send(), SOCKET_BUFFER_MIN);
        assert_eq!(limits.recv(), SOCKET_BUFFER_MAX);

        limits.set_send(64 * 1024);
        limits.set_recv(128 * 1024);
        assert_eq!(limits.send(), 64 * 1024);
        assert_eq!(limits.recv(), 128 * 1024);
    }
}
