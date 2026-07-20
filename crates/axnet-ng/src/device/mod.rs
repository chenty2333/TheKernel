use alloc::{string::String, vec::Vec};
use core::task::Waker;

use axerrno::{AxError, AxResult};
use axpoll::{PollRegistrationError, PollSet, RegisterError, RegistrationToken, UpdateError};
use axsync::spin::SpinNoIrq;
use smoltcp::{
    storage::PacketBuffer,
    time::Instant,
    wire::{IpAddress, IpCidr},
};

use crate::packet::{
    LinkHardwareType, PacketDeviceCapabilities, PacketDeviceContext, PacketSendRequest,
};

mod ethernet;
mod loopback;
mod veth;
#[cfg(feature = "vsock")]
mod vsock;

pub use ethernet::*;
pub use loopback::*;
pub use veth::*;
#[cfg(feature = "vsock")]
pub use vsock::*;

const ETHERNET_TYPE_MIN: u16 = 0x0600;
const ETHERNET_802_3_PROTOCOL: u16 = 0x0001;
const ETHERNET_802_2_PROTOCOL: u16 = 0x0004;

/// Describes whether a successful link-layer send queued receive work that
/// must be retired by this same network stack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketSendProgress {
    /// The send did not synchronously queue receive work for this stack.
    NoImmediateIngress,
    /// The send queued receive work that this stack must poll immediately.
    ImmediateIngressQueued,
}

/// Classifies a received Ethernet frame independently of transmit metadata.
///
/// Values below the Ethernet-II type range are length fields.  A leading
/// `0xffff` payload denotes the raw 802.3 form; other payloads use the 802.2
/// LLC protocol class.  Short payloads cannot carry that marker and therefore
/// also fall into the 802.2 class.
pub(crate) fn classify_ethernet_ingress_protocol(header_protocol: u16, payload: &[u8]) -> u16 {
    if header_protocol >= ETHERNET_TYPE_MIN {
        header_protocol
    } else if payload.starts_with(&[0xff, 0xff]) {
        ETHERNET_802_3_PROTOCOL
    } else {
        ETHERNET_802_2_PROTOCOL
    }
}

/// Retains exactly one bridge registration from a device-local wake source to
/// the stack-wide readiness source. A consumed one-shot token is replaced on
/// the next check/arm pass; an unchanged live token is updated in place.
pub(crate) struct DevicePollBridge {
    token: SpinNoIrq<Option<RegistrationToken>>,
}

impl DevicePollBridge {
    pub(crate) const fn new() -> Self {
        Self {
            token: SpinNoIrq::new(None),
        }
    }

    pub(crate) fn refresh(
        &self,
        source: &PollSet,
        waker: &Waker,
    ) -> Result<(), PollRegistrationError> {
        let mut token = self.token.lock();
        if let Some(current) = *token {
            match source.update(current, waker) {
                Ok(()) => return Ok(()),
                Err(UpdateError::InvalidToken) => *token = None,
                Err(UpdateError::Closed) => {
                    return Err(PollRegistrationError::Source {
                        index: 0,
                        error: RegisterError::Closed,
                    });
                }
            }
        }

        *token = Some(
            source
                .register(waker)
                .map_err(|error| PollRegistrationError::Source { index: 0, error })?,
        );
        Ok(())
    }

    pub(crate) fn cancel(&self, source: &PollSet) {
        if let Some(token) = self.token.lock().take() {
            source.cancel(token);
        }
    }
}

/// The link-layer class of a network interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterfaceKind {
    /// A software loopback interface.
    Loopback,
    /// An Ethernet-compatible interface.
    Ethernet,
}

/// A point-in-time description of one interface in a [`NetStack`](crate::NetStack).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterfaceInfo {
    /// Stable one-based interface index within the network stack.
    pub index: u32,
    /// Interface name.
    pub name: String,
    /// Link-layer class.
    pub kind: InterfaceKind,
    /// Maximum IP packet size accepted by the interface.
    pub mtu: usize,
    /// Link-layer address, when the device has one.
    pub hardware_address: Option<[u8; 6]>,
    /// Addresses configured on this interface.
    pub addresses: Vec<IpCidr>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceStats {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errors: u64,
    pub rx_dropped: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errors: u64,
    pub tx_dropped: u64,
}

impl DeviceStats {
    pub(crate) fn record_rx(&mut self, bytes: usize) {
        self.rx_bytes = self.rx_bytes.saturating_add(bytes as u64);
        self.rx_packets = self.rx_packets.saturating_add(1);
    }

    pub(crate) fn record_tx(&mut self, bytes: usize) {
        self.tx_bytes = self.tx_bytes.saturating_add(bytes as u64);
        self.tx_packets = self.tx_packets.saturating_add(1);
    }

    pub(crate) fn record_rx_error(&mut self) {
        self.rx_errors = self.rx_errors.saturating_add(1);
    }

    pub(crate) fn record_rx_drop(&mut self) {
        self.rx_dropped = self.rx_dropped.saturating_add(1);
    }

    pub(crate) fn record_tx_error(&mut self) {
        self.tx_errors = self.tx_errors.saturating_add(1);
    }

    pub(crate) fn record_tx_drop(&mut self) {
        self.tx_dropped = self.tx_dropped.saturating_add(1);
    }
}

pub trait Device: Send + Sync {
    fn name(&self) -> &str;
    fn stats(&self) -> DeviceStats;
    fn interface_kind(&self) -> InterfaceKind;
    fn mtu(&self) -> usize;

    fn hardware_address(&self) -> Option<[u8; 6]> {
        None
    }

    fn addresses(&self) -> Vec<IpCidr> {
        Vec::new()
    }

    /// Reports the packet-observation and injection operations this concrete
    /// device can actually perform.
    ///
    /// This is deliberately independent of [`InterfaceKind`]: an IP-only
    /// virtual device must not acquire raw-link semantics merely because it is
    /// presented as an Ethernet-style interface to configuration code.
    fn packet_capabilities(&self) -> PacketDeviceCapabilities {
        let hardware_type = match self.interface_kind() {
            InterfaceKind::Loopback => LinkHardwareType::Loopback,
            InterfaceKind::Ethernet => LinkHardwareType::Ethernet,
        };
        PacketDeviceCapabilities::unsupported(hardware_type)
    }

    fn recv(
        &mut self,
        context: PacketDeviceContext<'_>,
        buffer: &mut PacketBuffer<()>,
        timestamp: Instant,
    ) -> bool;
    /// Sends a packet to the next hop.
    ///
    /// Returns `true` if this operation resulted in the readiness of receive
    /// operation. This is true for loopback devices and can be used to speed
    /// up packet processing.
    fn send(
        &mut self,
        context: PacketDeviceContext<'_>,
        next_hop: IpAddress,
        packet: &[u8],
        timestamp: Instant,
    ) -> bool;

    /// Injects one packet through an explicitly advertised raw or cooked link
    /// capability. Unsupported devices fail without mutating their ordinary IP
    /// datapath. The result distinguishes ordinary device dispatch from
    /// immediate ingress queued for this same stack; it is not a hardware
    /// completion signal.
    fn send_packet(
        &mut self,
        _context: PacketDeviceContext<'_>,
        _request: PacketSendRequest<'_>,
        _timestamp: Instant,
    ) -> AxResult<PacketSendProgress> {
        Err(AxError::Unsupported)
    }

    /// Refreshes the retained bridge from this device's wake source to the
    /// stack readiness source.
    fn register_waker(&self, waker: &Waker) -> Result<(), PollRegistrationError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingress_protocol_preserves_ethernet_ii_types() {
        assert_eq!(
            classify_ethernet_ingress_protocol(0x0800, &[0x45, 0]),
            0x0800
        );
        assert_eq!(
            classify_ethernet_ingress_protocol(ETHERNET_TYPE_MIN, &[]),
            ETHERNET_TYPE_MIN
        );
    }

    #[test]
    fn ingress_protocol_classifies_length_frames_from_payload() {
        assert_eq!(
            classify_ethernet_ingress_protocol(0, &[0x01, 0x02]),
            ETHERNET_802_2_PROTOCOL
        );
        assert_eq!(
            classify_ethernet_ingress_protocol(1500, &[]),
            ETHERNET_802_2_PROTOCOL
        );
        assert_eq!(
            classify_ethernet_ingress_protocol(42, &[0xff, 0xff, 0x03]),
            ETHERNET_802_3_PROTOCOL
        );
    }
}
