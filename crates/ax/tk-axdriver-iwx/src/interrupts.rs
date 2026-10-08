//! Interrupt mask transitions from OpenBSD iwx.
//!
//! Upstream: OpenBSD `sys/dev/pci/if_iwx.c` revision 1.230,
//! `iwx_enable_rfkill_int()`, `iwx_enable_interrupts()`,
//! `iwx_enable_fwload_interrupt()`, and `iwx_check_rfkill()`. ISC.
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>
//!   Author: Stefan Sperling <stsp@openbsd.org>
//! Copyright (c) 2014 Fixup Software Ltd.
//! Copyright (c) 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>

use crate::{CsrAccess, IwxRegisters};

const CSR_INT_MASK: u32 = 0x00c;
const CSR_GP_CNTRL: u32 = 0x024;
const CSR_GP_RF_KILL_WAKE: u32 = 0x0400_0000;
const CSR_GP_HW_RF_KILL_SW: u32 = 0x0800_0000;
const CSR_CSR_INT_FH_RX: u32 = 1 << 31;
const CSR_CSR_INT_HW_ERR: u32 = 1 << 29;
const CSR_CSR_INT_RX_PERIODIC: u32 = 1 << 28;
const CSR_CSR_INT_FH_TX: u32 = 1 << 27;
const CSR_CSR_INT_SW_ERR: u32 = 1 << 25;
const CSR_CSR_INT_RF_KILL: u32 = 1 << 7;
const CSR_CSR_INT_SW_RX: u32 = 1 << 3;
const CSR_CSR_INT_WAKEUP: u32 = 1 << 1;
const CSR_CSR_INT_ALIVE: u32 = 1 << 0;
const CSR_CSR_INIT_SET_MASK: u32 = CSR_CSR_INT_FH_RX
    | CSR_CSR_INT_HW_ERR
    | CSR_CSR_INT_FH_TX
    | CSR_CSR_INT_SW_ERR
    | CSR_CSR_INT_RF_KILL
    | CSR_CSR_INT_SW_RX
    | CSR_CSR_INT_WAKEUP
    | CSR_CSR_INT_ALIVE
    | CSR_CSR_INT_RX_PERIODIC;
const CSR_MSIX_BASE: u32 = 0x2000;
const CSR_MSIX_FH_MASK: u32 = CSR_MSIX_BASE + 0x804;
const CSR_MSIX_HW_MASK: u32 = CSR_MSIX_BASE + 0x80c;
const MSIX_HW_CAUSE_ALIVE: u32 = 1 << 0;
const MSIX_HW_CAUSE_RF_KILL: u32 = 1 << 7;

/// Mutable OpenBSD mask state for the MSI/MSI-X interrupt paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InterruptMasks {
    pub msix: bool,
    pub interrupt_mask: u32,
    pub fh_init_mask: u32,
    pub fh_mask: u32,
    pub hw_init_mask: u32,
    pub hw_mask: u32,
}

/// Enable only the RF-kill wake path while the device is blocked.
// upstream: if_iwx.c iwx_enable_rfkill_int()
pub fn enable_rfkill_interrupts<B: CsrAccess>(
    registers: &mut IwxRegisters<B>,
    masks: &mut InterruptMasks,
) {
    if !masks.msix {
        masks.interrupt_mask = CSR_CSR_INT_RF_KILL;
        registers.write_csr(CSR_INT_MASK, masks.interrupt_mask);
    } else {
        registers.write_csr(CSR_MSIX_FH_MASK, masks.fh_init_mask);
        registers.write_csr(CSR_MSIX_HW_MASK, !MSIX_HW_CAUSE_RF_KILL);
        masks.hw_mask = MSIX_HW_CAUSE_RF_KILL;
    }
    registers.set_csr_bits(CSR_GP_CNTRL, CSR_GP_RF_KILL_WAKE);
}

/// Enable the normal interrupt sources after firmware/device initialization.
// upstream: if_iwx.c iwx_enable_interrupts()
pub fn enable_interrupts<B: CsrAccess>(
    registers: &mut IwxRegisters<B>,
    masks: &mut InterruptMasks,
) {
    if !masks.msix {
        masks.interrupt_mask = CSR_CSR_INIT_SET_MASK;
        registers.write_csr(CSR_INT_MASK, masks.interrupt_mask);
    } else {
        masks.hw_mask = masks.hw_init_mask;
        masks.fh_mask = masks.fh_init_mask;
        registers.write_csr(CSR_MSIX_FH_MASK, !masks.fh_mask);
        registers.write_csr(CSR_MSIX_HW_MASK, !masks.hw_mask);
    }
}

/// Enable only ALIVE and flow-handler RX events during firmware boot.
// upstream: if_iwx.c iwx_enable_fwload_interrupt()
pub fn enable_firmware_load_interrupts<B: CsrAccess>(
    registers: &mut IwxRegisters<B>,
    masks: &mut InterruptMasks,
) {
    if !masks.msix {
        masks.interrupt_mask = CSR_CSR_INT_ALIVE | CSR_CSR_INT_FH_RX;
        registers.write_csr(CSR_INT_MASK, masks.interrupt_mask);
    } else {
        registers.write_csr(CSR_MSIX_HW_MASK, !MSIX_HW_CAUSE_ALIVE);
        masks.hw_mask = MSIX_HW_CAUSE_ALIVE;
        registers.write_csr(CSR_MSIX_FH_MASK, !masks.fh_init_mask);
        masks.fh_mask = masks.fh_init_mask;
    }
}

/// Return `true` while the firmware-visible hardware RF-kill switch is asserted.
// upstream: if_iwx.c iwx_check_rfkill()
pub fn hardware_rfkill<B: CsrAccess>(registers: &mut IwxRegisters<B>) -> bool {
    registers.read_csr(CSR_GP_CNTRL) & CSR_GP_HW_RF_KILL_SW == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IoBarrier;

    #[derive(Default)]
    struct MockCsr {
        writes: alloc::vec::Vec<(u32, u32)>,
        gp_control: u32,
    }

    impl CsrAccess for MockCsr {
        fn read32(&mut self, offset: u32) -> u32 {
            if offset == CSR_GP_CNTRL {
                self.gp_control
            } else {
                0
            }
        }

        fn write32(&mut self, offset: u32, value: u32) {
            self.writes.push((offset, value));
            if offset == CSR_GP_CNTRL {
                self.gp_control = value;
            }
        }

        fn barrier(&mut self, _: IoBarrier) {}
        fn delay_us(&mut self, _: u32) {}
    }

    #[test]
    fn legacy_interrupt_masks_follow_firmware_phases() {
        let mut regs = IwxRegisters::new(MockCsr::default(), crate::DeviceFamily::Ax210, 0);
        let mut masks = InterruptMasks::default();

        enable_firmware_load_interrupts(&mut regs, &mut masks);
        assert_eq!(masks.interrupt_mask, CSR_CSR_INT_ALIVE | CSR_CSR_INT_FH_RX);
        enable_interrupts(&mut regs, &mut masks);
        assert_eq!(masks.interrupt_mask, CSR_CSR_INIT_SET_MASK);
        enable_rfkill_interrupts(&mut regs, &mut masks);
        assert_eq!(masks.interrupt_mask, CSR_CSR_INT_RF_KILL);
        assert_eq!(regs.into_inner().writes.len(), 4);
    }

    #[test]
    fn msix_masks_are_active_low_and_track_enabled_causes() {
        let mut regs = IwxRegisters::new(MockCsr::default(), crate::DeviceFamily::Ax210, 0);
        let mut masks = InterruptMasks {
            msix: true,
            fh_init_mask: 0x30,
            fh_mask: 0,
            hw_init_mask: 0x81,
            hw_mask: 0,
            interrupt_mask: 0,
        };

        enable_firmware_load_interrupts(&mut regs, &mut masks);
        assert_eq!(masks.hw_mask, MSIX_HW_CAUSE_ALIVE);
        assert_eq!(masks.fh_mask, 0x30);
        enable_interrupts(&mut regs, &mut masks);
        assert_eq!(masks.hw_mask, 0x81);
        enable_rfkill_interrupts(&mut regs, &mut masks);
        assert_eq!(masks.hw_mask, MSIX_HW_CAUSE_RF_KILL);
        let writes = regs.into_inner().writes;
        assert!(writes.contains(&(CSR_MSIX_HW_MASK, !MSIX_HW_CAUSE_ALIVE)));
        assert!(writes.contains(&(CSR_MSIX_FH_MASK, !0x30)));
        assert!(writes.contains(&(CSR_MSIX_HW_MASK, !0x81)));
        assert!(writes.contains(&(CSR_MSIX_HW_MASK, !MSIX_HW_CAUSE_RF_KILL)));
    }

    #[test]
    fn rfkill_reports_active_low_switch_state() {
        let mut mock = MockCsr::default();
        assert!(hardware_rfkill(&mut IwxRegisters::new(
            MockCsr::default(),
            crate::DeviceFamily::Ax210,
            0
        )));
        mock.gp_control = CSR_GP_HW_RF_KILL_SW;
        assert!(!hardware_rfkill(&mut IwxRegisters::new(
            mock,
            crate::DeviceFamily::Ax210,
            0
        )));
    }
}
