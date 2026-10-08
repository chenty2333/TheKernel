//! Common traits and types for network device (NIC) drivers.

// The crate is `no_std` in every kernel build; the host test build links `std`
// because the test harness needs it, and the pure logic in this crate -- the
// parts a machine without the hardware can still check -- lives where that
// harness can reach it.
#![cfg_attr(not(test), no_std)]
#![cfg_attr(doc, feature(doc_cfg))]

extern crate alloc;

#[cfg(test)]
extern crate std;

#[cfg(feature = "fxmac")]
/// fxmac driver for PhytiumPi
pub mod fxmac;
#[cfg(feature = "rtl8125")]
pub mod rtl8125;
#[cfg(feature = "igc")]
/// Intel i225/i226 (2.5 GbE) NIC device driver.
pub mod igc;
#[cfg(feature = "ixgbe")]
/// ixgbe NIC device driver.
pub mod ixgbe;

#[doc(no_inline)]
pub use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};

mod net_buf;
pub use self::net_buf::{NetBuf, NetBufBox, NetBufPool, NetBufPtr};

/// The ethernet address of the NIC (MAC address).
pub struct EthernetAddress(pub [u8; 6]);

/// One validated frequency made available by a wireless hardware radio.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WirelessFrequency {
    pub frequency_mhz: u32,
    pub no_ir: bool,
}

/// IEEE 802.11 HT capability bytes advertised by an 802.11 radio.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WirelessHtCapabilities {
    pub capability: u16,
    pub ampdu_parameters: u8,
    /// The UAPI 16-byte HT MCS information block.
    pub mcs_set: [u8; 16],
}

/// IEEE 802.11 VHT capability and MCS-map bytes for a radio.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WirelessVhtCapabilities {
    pub capability: u32,
    /// RX map/highest rate followed by TX map/highest rate (8 bytes).
    pub mcs_set: [u8; 8],
}

/// Negotiated local PHY capabilities exported to generic wireless users.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WirelessPhyCapabilities {
    pub ht: Option<WirelessHtCapabilities>,
    pub vht: Option<WirelessVhtCapabilities>,
}

/// Operations that require a network device (NIC) driver to implement.
pub trait NetDriverOps: BaseDriverOps {
    /// Preferred init-net interface name, when the driver owns a named link.
    /// `None` keeps the platform's existing primary-Ethernet naming policy.
    fn interface_name(&self) -> Option<&'static str> {
        None
    }

    /// Whether this Ethernet-compatible link exposes 802.11 control state.
    fn is_wireless(&self) -> bool {
        false
    }

    /// Current hardware RF-kill state, when this is a wireless interface.
    fn rfkill_hard_blocked(&self) -> bool {
        false
    }

    /// Current software RF-kill state, when this is a wireless interface.
    fn rfkill_soft_blocked(&self) -> bool {
        false
    }

    /// Change software rfkill state when this wireless adapter can serialize it.
    fn set_rfkill_soft_blocked(&mut self, _blocked: bool) -> DevResult {
        Err(DevError::Unsupported)
    }

    /// Frequencies validated by the device NVM/regulatory admission path.
    fn wireless_frequencies(&self) -> alloc::vec::Vec<WirelessFrequency> {
        alloc::vec::Vec::new()
    }

    /// HT/VHT capabilities admitted by the radio's NVM and local antenna policy.
    fn wireless_phy_capabilities(&self) -> WirelessPhyCapabilities {
        WirelessPhyCapabilities::default()
    }

    /// Change administrative radio state before the interface state is published.
    fn set_link_up(&mut self, _up: bool) -> DevResult {
        Ok(())
    }

    /// The ethernet address of the NIC.
    fn mac_address(&self) -> EthernetAddress;

    /// Whether can transmit packets.
    fn can_transmit(&self) -> bool;

    /// Whether can receive packets.
    fn can_receive(&self) -> bool;

    /// Size of the receive queue.
    fn rx_queue_size(&self) -> usize;

    /// Size of the transmit queue.
    fn tx_queue_size(&self) -> usize;

    /// Gives back the `rx_buf` to the receive queue for later receiving.
    ///
    /// `rx_buf` should be the same as the one returned by
    /// [`NetDriverOps::receive`].
    fn recycle_rx_buffer(&mut self, rx_buf: NetBufPtr) -> DevResult;

    /// Poll the transmit queue and gives back the buffers for previous transmiting.
    /// returns [`DevResult`].
    fn recycle_tx_buffers(&mut self) -> DevResult;

    /// Transmits a packet in the buffer to the network, without blocking,
    /// returns [`DevResult`].
    fn transmit(&mut self, tx_buf: NetBufPtr) -> DevResult;

    /// Receives a packet from the network and store it in the [`NetBuf`],
    /// returns the buffer.
    ///
    /// Before receiving, the driver should have already populated some buffers
    /// in the receive queue by [`NetDriverOps::recycle_rx_buffer`].
    ///
    /// If currently no incomming packets, returns an error with type
    /// [`DevError::Again`].
    fn receive(&mut self) -> DevResult<NetBufPtr>;

    /// Allocate a memory buffer of a specified size for network transmission,
    /// returns [`DevResult`]
    fn alloc_tx_buffer(&mut self, size: usize) -> DevResult<NetBufPtr>;

    /// Native drivers may retain a polling fallback even when IRQs are enabled.
    fn rx_poll_interval_micros(&self) -> Option<u64> { None }

    /// Optional firmware staged in rootfs, loaded before network publication.
    fn firmware_path(&self) -> Option<&'static str> { None }
    /// Apply validated runtime firmware while there are no packet borrowers.
    fn load_firmware(&mut self, _bytes: &[u8]) -> DevResult { Err(DevError::Unsupported) }

}

#[cfg(feature = "rtl8125")]
pub mod r8169;
