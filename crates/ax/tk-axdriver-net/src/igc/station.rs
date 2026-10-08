//! The station address retained across PCI setup and the network interface.

/// The station address observed after NVM auto-read, plus the source registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StationAddress {
    /// The six bytes assembled from `RAL(0)`/`RAH(0)`.
    pub bytes: [u8; 6],
    /// The raw `IGC_RAL(0)` value.
    pub low: u32,
    /// The raw `IGC_RAH(0)` value.
    pub high: u32,
    /// `IGC_RAH_AV`, which says the receive filter entry is armed.
    pub address_valid: bool,
}

impl StationAddress {
    /// Format the address for the probe report.
    pub fn describe(&self) -> alloc::string::String {
        alloc::format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.bytes[0],
            self.bytes[1],
            self.bytes[2],
            self.bytes[3],
            self.bytes[4],
            self.bytes[5],
        )
    }
}
