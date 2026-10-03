//! Identity facts from Linux 7.2.3 r8169_main.c rtl_chip_infos.
pub const VENDOR: u16 = 0x10ec;
pub const DEVICE: u16 = 0x8125;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    Rtl8125B,
    Rtl8168H,
}
impl Chip {
    pub fn name(self) -> &'static str {
        match self {
            Self::Rtl8125B => "rtl8125",
            Self::Rtl8168H => "rtl8168",
        }
    }
    pub fn window(self) -> usize {
        match self {
            Self::Rtl8125B => 0x10000,
            Self::Rtl8168H => 0x1000,
        }
    }
    pub fn irq(self) -> (usize, usize, super::regs::Width) {
        match self {
            Self::Rtl8125B => (0x38, 0x3c, super::regs::Width::Dword),
            Self::Rtl8168H => (0x3c, 0x3e, super::regs::Width::Word),
        }
    }
}
pub fn xid(tx_config: u32) -> u32 {
    (tx_config >> 20) & 0xfff
}
pub fn identify(tx_config: u32) -> Option<Chip> {
    match xid(tx_config) & 0x7cf {
        0x641 => Some(Chip::Rtl8125B),
        0x541 => Some(Chip::Rtl8168H),
        _ => None,
    }
}
pub fn matches(vendor: u16, device: u16) -> bool {
    vendor == VENDOR && matches!(device, DEVICE | 0x8168)
}
pub fn pci_chip(device: u16, tx_config: u32) -> Option<Chip> {
    let chip = identify(tx_config)?;
    match (device, chip) {
        (0x8125, Chip::Rtl8125B) | (0x8168, Chip::Rtl8168H) => Some(chip),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pci_revision_is_not_a_mac_identity() {
        assert!(matches(0x10ec, 0x8168));
        assert!(!matches(0x8086, 0x8168));
        assert_eq!(pci_chip(0x8168, 0x54100000), Some(Chip::Rtl8168H));
        assert_eq!(pci_chip(0x8125, 0x64100000), Some(Chip::Rtl8125B));
        assert_eq!(pci_chip(0x8168, 0x64100000), None);
        for value in [0, u32::MAX, 0x54000000, 0x60900000] {
            assert_eq!(identify(value), None);
        }
    }
}
