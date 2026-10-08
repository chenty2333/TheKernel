//! Card readiness and power-management sequence from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{
    CsrAccess, DeviceFamily, InterruptMasks, IwxRegisters, RegisterError, enable_rfkill_interrupts,
    hardware_rfkill,
};

const CSR_HW_IF_CONFIG: u32 = 0x000;
const CSR_GP_CNTRL: u32 = 0x024;
const CSR_RESET: u32 = 0x020;
const CSR_GIO_REG: u32 = 0x03c;
const CSR_MBOX_SET: u32 = 0x088;
const CSR_GIO_CHICKEN_BITS: u32 = 0x100;
const CSR_DBG_HPET_MEM: u32 = 0x240;
const CSR_DBG_LINK_PWR_MGMT: u32 = 0x250;
const HW_IF_CONFIG_NIC_READY: u32 = 0x0040_0000;
const HW_IF_CONFIG_HAP_WAKE_L1A: u32 = 0x0008_0000;
const HW_IF_CONFIG_PREPARE: u32 = 0x0800_0000;
const HW_IF_CONFIG_ENABLE_PME: u32 = 0x1000_0000;
const HW_IF_CONFIG_PREPARE_READY: u32 = HW_IF_CONFIG_PREPARE | HW_IF_CONFIG_ENABLE_PME;
const MBOX_SET_OS_ALIVE: u32 = 0x20;
const RESET_SW: u32 = 0x80;
const RESET_MASTER_DISABLED: u32 = 0x100;
const RESET_STOP_MASTER: u32 = 0x200;
const RESET_LINK_PWR_DISABLED: u32 = 0x8000_0000;
const GIO_L0S_DISABLED: u32 = 0x2;
const GIO_L1A_NO_L0S_RX: u32 = 0x0080_0000;
const DBG_HPET_MEM_VALUE: u32 = 0xffff_0000;
const GP_MAC_CLOCK_READY: u32 = 1;
const GP_INIT_DONE: u32 = 1 << 2;
const GP_MAC_INIT: u32 = 1 << 6;
const GP_MAC_STATUS: u32 = 1 << 20;
const GP_BUS_MASTER_DISABLED: u32 = 1 << 28;
const GP_BUS_MASTER_DISABLE_REQ: u32 = 1 << 29;
const GP_SW_RESET: u32 = 1 << 31;
const HPM_HIPM_GEN_CFG: u32 = 0x00a0_3458;
const HPM_CFG_CR_PG_EN: u32 = 1;
const HPM_CFG_CR_SLP_EN: u32 = 1 << 1;
const HPM_CFG_CR_FORCE_ACTIVE: u32 = 1 << 10;

/// A power-sequence operation failed before the device could be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApmError {
    Timeout,
    Register(RegisterError),
}

impl From<RegisterError> for ApmError {
    fn from(error: RegisterError) -> Self {
        Self::Register(error)
    }
}

/// Mark the NIC ready and announce OS ownership once hardware grants it.
// upstream: if_iwx.c iwx_set_hw_ready()
pub fn set_hw_ready<B: CsrAccess>(registers: &mut IwxRegisters<B>) -> bool {
    registers.set_csr_bits(CSR_HW_IF_CONFIG, HW_IF_CONFIG_NIC_READY);
    let ready = registers.poll_bit(
        CSR_HW_IF_CONFIG,
        HW_IF_CONFIG_NIC_READY,
        HW_IF_CONFIG_NIC_READY,
        50,
    );
    if ready {
        registers.set_csr_bits(CSR_MBOX_SET, MBOX_SET_OS_ALIVE);
    }
    ready
}

/// Retry the platform's NIC-ready handshake, preserving the upstream delays.
// upstream: if_iwx.c iwx_prepare_card_hw()
pub fn prepare_card_hw<B: CsrAccess>(registers: &mut IwxRegisters<B>) -> Result<(), ApmError> {
    if set_hw_ready(registers) {
        return Ok(());
    }
    registers.set_csr_bits(CSR_DBG_LINK_PWR_MGMT, RESET_LINK_PWR_DISABLED);
    registers.delay_us(1_000);
    let mut elapsed = 0;
    for _ in 0..10 {
        registers.set_csr_bits(CSR_HW_IF_CONFIG, HW_IF_CONFIG_PREPARE);
        while elapsed < 150_000 {
            if set_hw_ready(registers) {
                return Ok(());
            }
            registers.delay_us(200);
            elapsed += 200;
        }
        registers.delay_us(25_000);
    }
    Err(ApmError::Timeout)
}

/// Configure AX power-gating PRPH bits in the source order.
// upstream: if_iwx.c iwx_force_power_gating()
pub fn force_power_gating<B: CsrAccess>(registers: &mut IwxRegisters<B>) -> Result<(), ApmError> {
    registers.set_bits_prph(HPM_HIPM_GEN_CFG, HPM_CFG_CR_FORCE_ACTIVE)?;
    registers.delay_us(20);
    registers.set_bits_prph(HPM_HIPM_GEN_CFG, HPM_CFG_CR_PG_EN | HPM_CFG_CR_SLP_EN)?;
    registers.delay_us(20);
    registers.clear_bits_prph(HPM_HIPM_GEN_CFG, HPM_CFG_CR_FORCE_ACTIVE)?;
    Ok(())
}

/// Start basic NIC power after reset; does not load uCode.
// upstream: if_iwx.c iwx_apm_init()
pub fn apm_init<B: CsrAccess>(registers: &mut IwxRegisters<B>) -> Result<(), ApmError> {
    registers.set_csr_bits(CSR_GIO_CHICKEN_BITS, GIO_L1A_NO_L0S_RX);
    registers.set_csr_bits(CSR_DBG_HPET_MEM, DBG_HPET_MEM_VALUE);
    registers.set_csr_bits(CSR_HW_IF_CONFIG, HW_IF_CONFIG_HAP_WAKE_L1A);
    registers.set_csr_bits(CSR_GIO_REG, GIO_L0S_DISABLED);
    let (set, ready) = if registers.family() >= DeviceFamily::Bz {
        (GP_MAC_CLOCK_READY | GP_MAC_INIT, GP_MAC_STATUS)
    } else {
        (GP_INIT_DONE, GP_MAC_CLOCK_READY)
    };
    registers.set_csr_bits(CSR_GP_CNTRL, set);
    if registers.poll_bit(CSR_GP_CNTRL, ready, ready, 25_000) {
        Ok(())
    } else {
        Err(ApmError::Timeout)
    }
}

/// Shut down card power and stop busmaster DMA, returning master-stop status.
// upstream: if_iwx.c iwx_apm_stop()
pub fn apm_stop<B: CsrAccess>(registers: &mut IwxRegisters<B>) -> bool {
    registers.set_csr_bits(CSR_DBG_LINK_PWR_MGMT, RESET_LINK_PWR_DISABLED);
    registers.set_csr_bits(CSR_HW_IF_CONFIG, HW_IF_CONFIG_PREPARE_READY);
    registers.delay_us(1_000);
    registers.clear_csr_bits(CSR_DBG_LINK_PWR_MGMT, RESET_LINK_PWR_DISABLED);
    registers.delay_us(5_000);
    let master_stopped = if registers.family() >= DeviceFamily::Bz {
        registers.set_csr_bits(CSR_GP_CNTRL, GP_BUS_MASTER_DISABLE_REQ);
        let stopped = registers.poll_bit(
            CSR_GP_CNTRL,
            GP_BUS_MASTER_DISABLED,
            GP_BUS_MASTER_DISABLED,
            5_000,
        );
        registers.delay_us(20_000);
        stopped
    } else {
        registers.set_csr_bits(CSR_RESET, RESET_STOP_MASTER);
        registers.poll_bit(CSR_RESET, RESET_MASTER_DISABLED, RESET_MASTER_DISABLED, 100)
    };
    if registers.family() >= DeviceFamily::Bz {
        registers.clear_csr_bits(CSR_GP_CNTRL, GP_MAC_INIT);
    } else {
        registers.clear_csr_bits(CSR_GP_CNTRL, GP_INIT_DONE);
    }
    master_stopped
}

/// Trigger a generation-specific software reset.
// upstream: if_iwx.c iwx_sw_reset()
pub fn software_reset<B: CsrAccess>(registers: &mut IwxRegisters<B>) {
    if registers.family() >= DeviceFamily::Bz {
        registers.set_csr_bits(CSR_GP_CNTRL, GP_SW_RESET);
        registers.delay_us(20_000);
    } else {
        registers.set_csr_bits(CSR_RESET, RESET_SW);
        registers.delay_us(5_000);
    }
}

/// Prepare hardware, reset it, start APM, and enable RF-kill interrupts.
// upstream: if_iwx.c iwx_start_hw()
pub fn start_hardware<B: CsrAccess>(
    registers: &mut IwxRegisters<B>,
    masks: &mut InterruptMasks,
    integrated_22000: bool,
) -> Result<bool, ApmError> {
    prepare_card_hw(registers)?;
    software_reset(registers);
    if registers.family() == DeviceFamily::Family22000 && integrated_22000 {
        registers.set_csr_bits(CSR_GP_CNTRL, GP_INIT_DONE);
        registers.delay_us(20);
        if !registers.poll_bit(CSR_GP_CNTRL, GP_MAC_CLOCK_READY, GP_MAC_CLOCK_READY, 25_000) {
            return Err(ApmError::Timeout);
        }
        force_power_gating(registers)?;
        software_reset(registers);
    }
    apm_init(registers)?;
    enable_rfkill_interrupts(registers, masks);
    Ok(hardware_rfkill(registers))
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
        delays: u64,
    }

    impl CsrAccess for Bus {
        fn read32(&mut self, offset: u32) -> u32 {
            *self.regs.get(&offset).unwrap_or(&0)
        }
        fn write32(&mut self, offset: u32, value: u32) {
            self.writes.push((offset, value));
            self.regs.insert(offset, value);
        }
        fn barrier(&mut self, _: IoBarrier) {}
        fn delay_us(&mut self, micros: u32) {
            self.delays += u64::from(micros);
        }
    }

    #[test]
    fn ready_handshake_sets_os_alive_after_ready() {
        let mut regs = IwxRegisters::new(Bus::default(), DeviceFamily::Ax210, 0);
        assert!(set_hw_ready(&mut regs));
        let bus = regs.into_inner();
        assert_eq!(
            bus.regs[&CSR_HW_IF_CONFIG] & HW_IF_CONFIG_NIC_READY,
            HW_IF_CONFIG_NIC_READY
        );
        assert_eq!(
            bus.regs[&CSR_MBOX_SET] & MBOX_SET_OS_ALIVE,
            MBOX_SET_OS_ALIVE
        );
    }

    #[test]
    fn apm_init_waits_for_generation_clock_and_sets_common_bits() {
        let mut bus = Bus::default();
        bus.regs.insert(CSR_GP_CNTRL, GP_MAC_CLOCK_READY);
        let mut regs = IwxRegisters::new(bus, DeviceFamily::Ax210, 0);
        apm_init(&mut regs).unwrap();
        let bus = regs.into_inner();
        assert_eq!(bus.regs[&CSR_GP_CNTRL] & GP_INIT_DONE, GP_INIT_DONE);
        assert_eq!(bus.regs[&CSR_GIO_REG] & GIO_L0S_DISABLED, GIO_L0S_DISABLED);
        assert!(
            bus.writes
                .contains(&(CSR_GIO_CHICKEN_BITS, GIO_L1A_NO_L0S_RX))
        );
    }

    #[test]
    fn start_hardware_runs_reset_and_rfkill_phase_for_ax210() {
        let mut bus = Bus::default();
        bus.regs.insert(CSR_GP_CNTRL, GP_MAC_CLOCK_READY);
        let mut regs = IwxRegisters::new(bus, DeviceFamily::Ax210, 0);
        let mut masks = InterruptMasks::default();
        assert!(start_hardware(&mut regs, &mut masks, false).unwrap());
        assert_eq!(masks.interrupt_mask, 1 << 7);
        let bus = regs.into_inner();
        assert!(bus.writes.contains(&(CSR_RESET, RESET_SW)));
        assert!(bus.delays >= 5_000);
    }
}
