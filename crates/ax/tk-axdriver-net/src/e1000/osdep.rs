//! Intel e1000 OS-dependent register and PCI access adapters.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_osdep.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::registers::{
    E1000_FWSM, E1000_RCTL, E1000_RCTL_EN, E1000_TCTL, E1000_TCTL_EN, rx_desc_tail, tx_desc_tail,
};

const E1000_ICH_FWSM_PCIM2PCI_COUNT: usize = 2000;
const PCI_COMMAND: u32 = 0x04;
const PCI_COMMAND_MEM_WRITE_INVALIDATE: u16 = 0x0010;
const PCI_CAP_ID_EXPRESS: u8 = 0x10;
// upstream: e1000_ich8lan.h E1000_ICH_FWSM_PCIM2PCI
const E1000_ICH_FWSM_PCIM2PCI: u32 = 0x0100_0000;

/// Raw register access used by the 82579 Management Engine/PCIm2PCI workaround.
pub trait E1000RegisterIo {
    fn read_register(&mut self, register: u32) -> DevResult<u32>;
    fn write_register(&mut self, register: u32, value: u32) -> DevResult;
    fn delay_us(&mut self, micros: u32);
    fn invalid_tail_write(&mut self, direction: &'static str);
}

/// Width-correct PCI configuration access for the shared Intel helpers.
pub trait E1000PciConfig {
    fn read_config_u16(&mut self, register: u32) -> Option<u16>;
    fn write_config_u16(&mut self, register: u32, value: u16) -> bool;
    fn find_capability(&mut self, capability_id: u8) -> Option<u32>;
}

// upstream: e1000_osdep.c e1000_pcim2pci_arbiter_wait()
pub fn pcim2pci_arbiter_wait<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let mut remaining = E1000_ICH_FWSM_PCIM2PCI_COUNT;
    while io.read_register(E1000_FWSM)? & E1000_ICH_FWSM_PCIM2PCI != 0 && remaining > 1 {
        remaining -= 1;
        io.delay_us(50);
    }
    Ok(())
}

// upstream: e1000_osdep.c e1000_pcim2pci_write()
pub fn pcim2pci_write<I: E1000RegisterIo>(io: &mut I, register: u32, value: u32) -> DevResult {
    pcim2pci_arbiter_wait(io)?;
    io.write_register(register, value)?;
    let (control_register, enable, direction) = if register == tx_desc_tail(0) {
        (E1000_TCTL, E1000_TCTL_EN, "transmit")
    } else if register == rx_desc_tail(0) {
        (E1000_RCTL, E1000_RCTL_EN, "receive")
    } else {
        return Ok(());
    };
    if io.read_register(register)? == value {
        return Ok(());
    }

    let control = io.read_register(control_register)?;
    pcim2pci_arbiter_wait(io)?;
    io.write_register(control_register, control & !enable)?;
    io.invalid_tail_write(direction);
    Err(DevError::Io)
}

// upstream: e1000_osdep.c e1000_write_pci_cfg()
pub fn write_pci_cfg<P: E1000PciConfig>(pci: &mut P, register: u32, value: u16) -> DevResult {
    pci.write_config_u16(register, value)
        .then_some(())
        .ok_or(DevError::Io)
}

// upstream: e1000_osdep.c e1000_read_pci_cfg()
pub fn read_pci_cfg<P: E1000PciConfig>(pci: &mut P, register: u32) -> DevResult<u16> {
    pci.read_config_u16(register).ok_or(DevError::Io)
}

// upstream: e1000_osdep.c e1000_pci_set_mwi()
pub fn pci_set_mwi<P: E1000PciConfig>(pci: &mut P, command_word: u16) -> DevResult {
    write_pci_cfg(
        pci,
        PCI_COMMAND,
        command_word | PCI_COMMAND_MEM_WRITE_INVALIDATE,
    )
}

// upstream: e1000_osdep.c e1000_pci_clear_mwi()
pub fn pci_clear_mwi<P: E1000PciConfig>(pci: &mut P, command_word: u16) -> DevResult {
    write_pci_cfg(
        pci,
        PCI_COMMAND,
        command_word & !PCI_COMMAND_MEM_WRITE_INVALIDATE,
    )
}

// upstream: e1000_osdep.c e1000_read_pcie_cap_reg()
pub fn read_pcie_cap_reg<P: E1000PciConfig>(pci: &mut P, register: u32) -> DevResult<u16> {
    let offset = pci
        .find_capability(PCI_CAP_ID_EXPRESS)
        .filter(|offset| *offset <= u8::MAX as u32)
        .ok_or(DevError::Unsupported)?;
    read_pci_cfg(
        pci,
        offset.checked_add(register).ok_or(DevError::InvalidParam)?,
    )
}

// upstream: e1000_osdep.c e1000_write_pcie_cap_reg()
pub fn write_pcie_cap_reg<P: E1000PciConfig>(pci: &mut P, register: u32, value: u16) -> DevResult {
    let offset = pci
        .find_capability(PCI_CAP_ID_EXPRESS)
        .filter(|offset| *offset <= u8::MAX as u32)
        .ok_or(DevError::Unsupported)?;
    write_pci_cfg(
        pci,
        offset.checked_add(register).ok_or(DevError::InvalidParam)?,
        value,
    )
}

/// FreeBSD `SYSINIT(enable_pause_delay)` is module-loader glue. TheKernel's
/// `E1000Hal::delay_us` is always a bounded busy wait, so there is no mutable
/// pause-delay global to initialize here.
pub const USE_PAUSE_DELAY: bool = false;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockRegisterIo {
        fwsm: u32,
        registers: [u32; 4],
        ignored_write: Option<u32>,
        last_reset_direction: Option<&'static str>,
        delays: usize,
    }

    impl MockRegisterIo {
        fn index(register: u32) -> Option<usize> {
            match register {
                register if register == tx_desc_tail(0) => Some(0),
                E1000_TCTL => Some(1),
                register if register == rx_desc_tail(0) => Some(2),
                E1000_RCTL => Some(3),
                _ => None,
            }
        }
    }

    impl E1000RegisterIo for MockRegisterIo {
        fn read_register(&mut self, register: u32) -> DevResult<u32> {
            if register == E1000_FWSM {
                return Ok(self.fwsm);
            }
            let index = Self::index(register).ok_or(DevError::InvalidParam)?;
            Ok(self.registers[index])
        }
        fn write_register(&mut self, register: u32, value: u32) -> DevResult {
            if self.ignored_write == Some(register) {
                return Ok(());
            }
            let index = Self::index(register).ok_or(DevError::InvalidParam)?;
            self.registers[index] = value;
            Ok(())
        }
        fn delay_us(&mut self, _: u32) {
            self.delays += 1;
        }
        fn invalid_tail_write(&mut self, direction: &'static str) {
            self.last_reset_direction = Some(direction);
        }
    }

    struct MockPci {
        configuration: [u16; 128],
        pcie: Option<u32>,
    }

    impl Default for MockPci {
        fn default() -> Self {
            Self {
                configuration: [0; 128],
                pcie: None,
            }
        }
    }

    impl E1000PciConfig for MockPci {
        fn read_config_u16(&mut self, register: u32) -> Option<u16> {
            self.configuration.get(register as usize / 2).copied()
        }
        fn write_config_u16(&mut self, register: u32, value: u16) -> bool {
            let Some(word) = self.configuration.get_mut(register as usize / 2) else {
                return false;
            };
            *word = value;
            true
        }
        fn find_capability(&mut self, capability_id: u8) -> Option<u32> {
            (capability_id == PCI_CAP_ID_EXPRESS)
                .then_some(self.pcie)
                .flatten()
        }
    }

    #[test]
    fn pcim2pci_tail_mismatch_disables_direction_and_requests_reset() {
        let mut io = MockRegisterIo {
            ignored_write: Some(tx_desc_tail(0)),
            registers: [0, E1000_TCTL_EN | 0x100, 0, 0],
            ..MockRegisterIo::default()
        };
        assert!(matches!(
            pcim2pci_write(&mut io, tx_desc_tail(0), 7),
            Err(DevError::Io)
        ));
        assert_eq!(io.registers[1], 0x100);
        assert_eq!(io.last_reset_direction, Some("transmit"));
    }

    #[test]
    fn pci_config_and_pcie_helpers_preserve_mwi_bits() {
        let mut pci = MockPci {
            pcie: Some(0x40),
            ..MockPci::default()
        };
        pci.configuration[PCI_COMMAND as usize / 2] = 0x0006;
        pci.configuration[0x48 / 2] = 0x1234;
        pci_set_mwi(&mut pci, 0x0006).unwrap();
        assert_eq!(read_pci_cfg(&mut pci, PCI_COMMAND).unwrap(), 0x0016);
        pci_clear_mwi(&mut pci, 0x0016).unwrap();
        assert_eq!(read_pcie_cap_reg(&mut pci, 8).unwrap(), 0x1234);
        write_pcie_cap_reg(&mut pci, 8, 0x5678).unwrap();
        assert_eq!(read_pcie_cap_reg(&mut pci, 8).unwrap(), 0x5678);
    }
}
