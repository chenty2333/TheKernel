//! NIC hardware setup following OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{ApmError, CsrAccess, DeviceFamily, IwxRegisters, apm_init};

const CSR_HW_IF_CONFIG: u32 = 0x000;
const CSR_INT_COALESCING: u32 = 0x004;
const CSR_MAC_SHADOW_REG_CTRL: u32 = 0x0a8;
const HOST_INT_TIMEOUT_DEFAULT: u8 = 0x40;
const HW_CONFIG_MAC_DASH_MASK: u32 = 0x3;
const HW_CONFIG_MAC_STEP_MASK: u32 = 0x0c;
const HW_CONFIG_MAC_SI: u32 = 0x100;
const HW_CONFIG_RADIO_SI: u32 = 0x200;
const HW_CONFIG_PHY_TYPE_MASK: u32 = 0x0c00;
const HW_CONFIG_PHY_DASH_MASK: u32 = 0x3000;
const HW_CONFIG_PHY_STEP_MASK: u32 = 0xc000;
const HW_CONFIG_MAC_DASH_POS: u32 = 0;
const HW_CONFIG_MAC_STEP_POS: u32 = 2;
const HW_CONFIG_PHY_TYPE_POS: u32 = 10;
const HW_CONFIG_PHY_DASH_POS: u32 = 12;
const HW_CONFIG_PHY_STEP_POS: u32 = 14;
const HW_CONFIG_FIELD_RADIO_TYPE: u32 = 0x3;
const HW_CONFIG_FIELD_RADIO_STEP: u32 = 0x3 << 2;
const HW_CONFIG_FIELD_RADIO_DASH: u32 = 0x3 << 4;

/// Configure MAC stepping/dash and firmware-derived radio type/step/dash.
// upstream: if_iwx.c iwx_nic_config()
pub fn configure_nic<B: CsrAccess>(
    registers: &mut IwxRegisters<B>,
    firmware_phy_config: u32,
    hardware_revision: u32,
) {
    let radio_type = firmware_phy_config & HW_CONFIG_FIELD_RADIO_TYPE;
    let radio_step = (firmware_phy_config & HW_CONFIG_FIELD_RADIO_STEP) >> 2;
    let radio_dash = (firmware_phy_config & HW_CONFIG_FIELD_RADIO_DASH) >> 4;
    let mac_step = (hardware_revision & 0x0c) >> 2;
    let mac_dash = hardware_revision & 0x3;
    let value = (mac_step << HW_CONFIG_MAC_STEP_POS)
        | (mac_dash << HW_CONFIG_MAC_DASH_POS)
        | (radio_type << HW_CONFIG_PHY_TYPE_POS)
        | (radio_step << HW_CONFIG_PHY_STEP_POS)
        | (radio_dash << HW_CONFIG_PHY_DASH_POS);
    let mask = HW_CONFIG_MAC_DASH_MASK
        | HW_CONFIG_MAC_STEP_MASK
        | HW_CONFIG_PHY_STEP_MASK
        | HW_CONFIG_PHY_DASH_MASK
        | HW_CONFIG_PHY_TYPE_MASK
        | HW_CONFIG_RADIO_SI
        | HW_CONFIG_MAC_SI;
    let current = registers.read_csr(CSR_HW_IF_CONFIG);
    registers.write_csr(CSR_HW_IF_CONFIG, (current & !mask) | value);
}

/// Set interrupt coalescing; RFH is configured by firmware after ALIVE.
// upstream: if_iwx.c iwx_nic_rx_init()
pub fn initialize_rx<B: CsrAccess>(registers: &mut IwxRegisters<B>) {
    registers.write_csr8(CSR_INT_COALESCING, HOST_INT_TIMEOUT_DEFAULT);
}

/// Start APM, configure pre-AX210 radio fields, and initialize RX interrupts.
// upstream: if_iwx.c iwx_nic_init()
pub fn initialize_nic<B: CsrAccess>(
    registers: &mut IwxRegisters<B>,
    firmware_phy_config: u32,
    hardware_revision: u32,
) -> Result<(), ApmError> {
    // The upstream function deliberately ignores this return value.
    let _ = apm_init(registers);
    if registers.family() < DeviceFamily::Ax210 {
        configure_nic(registers, firmware_phy_config, hardware_revision);
    }
    initialize_rx(registers);
    registers.set_csr_bits(CSR_MAC_SHADOW_REG_CTRL, 0x800f_ffff);
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::{collections::BTreeMap, vec::Vec};

    use super::*;
    use crate::IoBarrier;

    #[derive(Default)]
    struct Bus {
        regs: BTreeMap<u32, u32>,
        writes: Vec<(u32, u32)>,
    }
    impl CsrAccess for Bus {
        fn read32(&mut self, offset: u32) -> u32 {
            *self.regs.get(&offset).unwrap_or(&0)
        }
        fn write32(&mut self, offset: u32, value: u32) {
            self.regs.insert(offset, value);
            self.writes.push((offset, value));
        }
        fn write8(&mut self, offset: u32, value: u8) {
            self.regs.insert(offset, u32::from(value));
            self.writes.push((offset, u32::from(value)));
        }
        fn barrier(&mut self, _: IoBarrier) {}
        fn delay_us(&mut self, _: u32) {}
    }

    #[test]
    fn nic_config_preserves_unrelated_bits_and_uses_revision_radio_fields() {
        let mut bus = Bus::default();
        bus.regs
            .insert(CSR_HW_IF_CONFIG, 0x8000_0000 | HW_CONFIG_MAC_SI | 0x5555);
        let mut registers = IwxRegisters::new(bus, DeviceFamily::Family22000, 0);
        configure_nic(&mut registers, 0x31, 0x0d);
        let value = registers.into_inner().regs[&CSR_HW_IF_CONFIG];
        let expected = (0x8000_0000 | 0x5555)
            & !(HW_CONFIG_MAC_DASH_MASK
                | HW_CONFIG_MAC_STEP_MASK
                | HW_CONFIG_PHY_STEP_MASK
                | HW_CONFIG_PHY_DASH_MASK
                | HW_CONFIG_PHY_TYPE_MASK
                | HW_CONFIG_RADIO_SI
                | HW_CONFIG_MAC_SI)
            | (3 << HW_CONFIG_MAC_STEP_POS)
            | (1 << HW_CONFIG_MAC_DASH_POS)
            | (1 << HW_CONFIG_PHY_TYPE_POS)
            | (0 << HW_CONFIG_PHY_STEP_POS)
            | (3 << HW_CONFIG_PHY_DASH_POS);
        assert_eq!(value, expected);
    }

    #[test]
    fn nic_start_sets_rx_coalescing_and_shadow_control() {
        let mut bus = Bus::default();
        bus.regs.insert(0x024, 0);
        let mut registers = IwxRegisters::new(bus, DeviceFamily::Ax210, 0);
        initialize_nic(&mut registers, 0x31, 0x0d).unwrap();
        let bus = registers.into_inner();
        assert!(
            bus.writes
                .contains(&(CSR_INT_COALESCING, u32::from(HOST_INT_TIMEOUT_DEFAULT)))
        );
        assert_eq!(bus.regs[&CSR_MAC_SHADOW_REG_CTRL], 0x800f_ffff);
        let config_mask = HW_CONFIG_MAC_DASH_MASK
            | HW_CONFIG_MAC_STEP_MASK
            | HW_CONFIG_PHY_STEP_MASK
            | HW_CONFIG_PHY_DASH_MASK
            | HW_CONFIG_PHY_TYPE_MASK
            | HW_CONFIG_RADIO_SI
            | HW_CONFIG_MAC_SI;
        assert_eq!(bus.regs[&CSR_HW_IF_CONFIG] & config_mask, 0);
    }
}
