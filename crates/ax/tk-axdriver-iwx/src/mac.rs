//! CSR/OTP station-address extraction from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC).
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c) 2014
//! Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

use crate::{CsrAccess, IwxRegisters};

pub type MacAddress = [u8; 6];

/// Reverse the CSR register word byte order used by `iwx_flip_hw_address()`.
// upstream: if_iwx.c iwx_flip_hw_address()
pub fn flip_hardware_address(mac_addr0: u32, mac_addr1: u32) -> MacAddress {
    let first = mac_addr0.to_le_bytes();
    let second = mac_addr1.to_le_bytes();
    [first[3], first[2], first[1], first[0], second[1], second[0]]
}

/// Reject the known reserved, broadcast, zero, and multicast addresses.
// upstream: if_iwx.c iwx_is_valid_mac_addr()
pub fn is_valid_mac_address(address: &MacAddress) -> bool {
    const RESERVED: MacAddress = [0x02, 0xcc, 0xaa, 0xff, 0xee, 0x00];
    *address != RESERVED && *address != [0xff; 6] && *address != [0; 6] && address[0] & 1 == 0
}

/// Prefer a valid OEM strap address, otherwise use the OTP station address.
// upstream: if_iwx.c iwx_set_mac_addr_from_csr()
pub fn select_csr_mac_address(
    nic_lock_acquired: bool,
    strap: (u32, u32),
    otp: (u32, u32),
) -> Option<MacAddress> {
    if !nic_lock_acquired {
        return None;
    }
    let strap = flip_hardware_address(strap.0, strap.1);
    if is_valid_mac_address(&strap) {
        return Some(strap);
    }
    let otp = flip_hardware_address(otp.0, otp.1);
    is_valid_mac_address(&otp).then_some(otp)
}

/// Read strap and OTP words while holding the NIC lock, preferring a valid OEM address.
// upstream: if_iwx.c iwx_set_mac_addr_from_csr()
pub fn read_csr_mac_address<B: CsrAccess>(
    registers: &mut IwxRegisters<B>,
    address_base: u32,
) -> Option<MacAddress> {
    if registers.nic_lock().is_err() {
        return None;
    }
    let otp = (
        registers.read_csr(address_base),
        registers.read_csr(address_base + 4),
    );
    let strap = (
        registers.read_csr(address_base + 8),
        registers.read_csr(address_base + 12),
    );
    let unlocked = registers.nic_unlock();
    select_csr_mac_address(unlocked, strap, otp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csr_register_byte_order_and_strap_fallback_match_source() {
        assert_eq!(
            flip_hardware_address(0x0011_2233, 0x0000_4455),
            [0, 0x11, 0x22, 0x33, 0x44, 0x55]
        );
        let strap = (0x0011_2233, 0x0000_4455);
        let otp = (0x0066_7788, 0x0000_99aa);
        assert_eq!(
            select_csr_mac_address(true, strap, otp),
            Some([0, 0x11, 0x22, 0x33, 0x44, 0x55])
        );
        assert_eq!(select_csr_mac_address(false, strap, otp), None);
        assert_eq!(
            select_csr_mac_address(true, (u32::MAX, u32::MAX), otp),
            Some([0, 0x66, 0x77, 0x88, 0x99, 0xaa])
        );
    }

    #[test]
    fn address_validity_checks_reserved_and_multicast_values() {
        assert!(is_valid_mac_address(&[0, 0x11, 0x22, 0x33, 0x44, 0x55]));
        assert!(!is_valid_mac_address(&[0x02, 0xcc, 0xaa, 0xff, 0xee, 0]));
        assert!(!is_valid_mac_address(&[0xff; 6]));
        assert!(!is_valid_mac_address(&[0; 6]));
        assert!(!is_valid_mac_address(&[1, 2, 3, 4, 5, 6]));
    }

    #[test]
    fn csr_reader_holds_nic_lock_and_prefers_valid_strap_words() {
        use alloc::collections::BTreeMap;

        struct Bus(BTreeMap<u32, u32>);
        impl CsrAccess for Bus {
            fn read32(&mut self, offset: u32) -> u32 {
                *self.0.get(&offset).unwrap_or(&0)
            }
            fn write32(&mut self, offset: u32, value: u32) {
                self.0.insert(offset, value);
            }
            fn write8(&mut self, offset: u32, value: u8) {
                self.write32(offset, u32::from(value));
            }
            fn barrier(&mut self, _: crate::IoBarrier) {}
            fn delay_us(&mut self, _: u32) {}
        }

        let mut values = BTreeMap::new();
        values.insert(0x024, 1);
        values.insert(0x100, 0x0066_7788);
        values.insert(0x104, 0x0000_99aa);
        values.insert(0x108, 0x0011_2233);
        values.insert(0x10c, 0x0000_4455);
        let mut registers = IwxRegisters::new(Bus(values), crate::DeviceFamily::Ax210, 0);
        assert_eq!(
            read_csr_mac_address(&mut registers, 0x100),
            Some([0, 0x11, 0x22, 0x33, 0x44, 0x55])
        );
        assert_eq!(registers.nic_lock_count(), 0);
    }
}
