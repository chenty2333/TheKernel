//! Identification facts: Linux r8169_main.c:126, and the PCI RTL8125 ID.
pub const VENDOR: u16 = 0x10ec;
pub const DEVICE: u16 = 0x8125;
pub fn matches(vendor: u16, device: u16) -> bool {
    vendor == VENDOR && device == DEVICE
}
/// Only the RTL8125B/BG MAC revision is admitted. Newer 8125 variants have
/// different initialization and must not silently inherit this sequence.
pub fn is_b(tx_config: u32) -> bool {
    (tx_config >> 20) & 0x7cf == 0x641
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn b_revision_is_not_a_generic_8125_claim() {
        assert!(matches(0x10ec, 0x8125));
        assert!(!matches(0x10ec, 0x8168));
        assert!(is_b(0x64100000));
        assert!(!is_b(0x60900000));
        assert!(!is_b(u32::MAX));
    }
}
