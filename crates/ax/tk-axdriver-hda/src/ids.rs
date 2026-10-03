//! HDA compatibility interface, including Intel 8086:54c8 and QEMU ICH9.
pub const fn matches(vendor: u16, class: u8, subclass: u8, interface: u8) -> bool {
    vendor == 0x8086 && class == 4 && subclass == 3 && interface == 0
}
