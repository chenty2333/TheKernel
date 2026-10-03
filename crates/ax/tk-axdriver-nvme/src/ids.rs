//! Bind the standard NVM PCI interface, not a vendor-specific SSD model.
pub const fn matches(class: u8, subclass: u8, interface: u8) -> bool {
    class == 1 && subclass == 8 && interface == 2
}
