//! The shared hardware helpers are referenced by the `irq` and `legacy`
//! interface implementations; a feature-less build keeps them as a hardware
//! API surface.
#![allow(dead_code)]

//! BCM2836 CPU-local interrupt controller + BCM2835-style ARM control
//! interrupt controller.
//!
//! The Zero 2W (BCM2837 die) has two chained controllers:
//!
//! * `local` (`0x3f00_b000`): per-CPU interrupts — the ARM generic timer
//!   (CNTPNSIRQ = local IRQ 1), the GPU interrupt (local IRQ 8, which chains
//!   the banked controller below), and the PMU (local IRQ 9). Each CPU has
//!   its own pending/enable register set.
//! * `armctrl` (`0x3f00_b200`): three banks of 32 GPU/peripheral interrupts.
//!   Bank 0 holds 8 "basic" bits plus bank-overflow bits 8/9 and shortcut
//!   alias bits 10-20 (bank 1 bits 7/9/10/18/19 and bank 2 bits
//!   21/22/23/24/25/30).
//!
//! Peripheral IRQs are mapped into the legacy IRQ domain; the per-CPU timer
//! lives in the CPU-local domain.

use core::ptr::{read_volatile, write_volatile};

#[cfg(all(not(feature = "legacy"), feature = "irq"))]
use ax_plat::irq::{
    CPU_LOCAL_IRQ_DOMAIN, HwIrq, IpiTarget, IrqAffinity, IrqError, IrqId, IrqIf, IrqSource,
    TrapVector, dispatch_irq, legacy_irq, legacy_irq_raw,
};

use crate::config::*;

/// Reads a 32-bit MMIO register (platform helpers used by other modules).
#[inline]
pub(crate) fn mmio_read(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

/// Writes a 32-bit MMIO register.
#[inline]
pub(crate) fn mmio_write(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}

pub(crate) const fn local_addr(offset: usize) -> usize {
    LOCAL_INTC_PADDR + PHYS_VIRT_OFFSET + offset
}

/// Enables or disables a peripheral IRQ by its raw armctrl number (shared
/// with the legacy interface). Returns `false` for invalid IRQ numbers.
pub(crate) fn set_enable_shared(raw: usize, enabled: bool) -> bool {
    if raw >= 96 || (raw / 32 == 0 && raw >= 8) {
        return false;
    }
    let bank = raw / 32;
    let bit = 1u32 << (raw % 32);
    let reg = if enabled {
        armc_enable_reg(bank)
    } else {
        armc_disable_reg(bank)
    };
    mmio_write(armc_addr(reg), bit);
    true
}

/// Enables or disables the CPU-local timer interrupt (shared with the legacy
/// interface).
pub(crate) fn timer_irq_legacy_enable(enabled: bool) {
    let reg = local_addr(LOCAL_TIMER_INT_CONTROL0);
    let value = mmio_read(reg);
    if enabled {
        mmio_write(reg, value | (1 << LOCAL_IRQ_CNTPNSIRQ));
    } else {
        mmio_write(reg, value & !(1 << LOCAL_IRQ_CNTPNSIRQ));
    }
}

/// Initializes both interrupt controllers (shared with the legacy interface).
pub(crate) fn init_boot_irqs_shared() {
    // Local controller: route the GPU IRQ to CPU 0 and disable PMU.
    mmio_write(local_addr(LOCAL_CONTROL), 1);
    mmio_write(local_addr(0x00c), 0); // LOCAL_GPU_ROUTING
    mmio_write(local_addr(0x014), 0xf); // LOCAL_PM_ROUTING_CLR

    // Disable all peripheral interrupts in the banked controller.
    mmio_write(armc_addr(ARMC_BANK0_DISABLE), 0xffff_ffff);
    mmio_write(armc_addr(ARMC_BANK1_DISABLE), 0xffff_ffff);
    mmio_write(armc_addr(ARMC_BANK2_DISABLE), 0xffff_ffff);
    mmio_write(armc_addr(0x00c), 0); // REG_FIQ_CONTROL

    // Enable the timer interrupt on the local controller.
    timer_irq_legacy_enable(true);
}

#[cfg(all(not(feature = "legacy"), feature = "irq"))]
fn init_boot_irqs_result() -> Result<(), IrqError> {
    init_boot_irqs_shared();
    Ok(())
}

const fn armc_addr(offset: usize) -> usize {
    ARMCTRL_IC_PADDR + PHYS_VIRT_OFFSET + offset
}

/// The timer IRQ id in the CPU-local domain.
#[cfg(all(not(feature = "legacy"), feature = "irq"))]
pub(crate) fn timer_irq() -> IrqId {
    IrqId::new(CPU_LOCAL_IRQ_DOMAIN, HwIrq(LOCAL_IRQ_CNTPNSIRQ))
}

fn armc_enable_reg(bank: usize) -> usize {
    match bank {
        0 => ARMC_BANK0_ENABLE,
        1 => ARMC_BANK1_ENABLE,
        _ => ARMC_BANK2_ENABLE,
    }
}

fn armc_disable_reg(bank: usize) -> usize {
    match bank {
        0 => ARMC_BANK0_DISABLE,
        1 => ARMC_BANK1_DISABLE,
        _ => ARMC_BANK2_DISABLE,
    }
}

/// Scans all pending peripheral IRQs from the armctrl banks, honoring the
/// bank 0 shortcut aliases. Returns the raw IRQ numbers (0..96).
pub(crate) fn scan_armctrl_pending() -> alloc::vec::Vec<usize> {
    let mut pending = alloc::vec::Vec::new();
    let b0 = mmio_read(armc_addr(ARMC_BANK0_PENDING)) & BANK0_VALID_MASK;
    if b0 == 0 {
        return pending;
    }

    // Basic interrupts (bank 0, bits 0-7).
    for n in 0..8 {
        if b0 & (1 << n) != 0 {
            pending.push(n);
        }
    }
    // Shortcut aliases: bank 0 bits 10-14 -> bank 1 bits {7,9,10,18,19}.
    if b0 & SHORTCUT_MASK1 != 0 {
        for (i, &bit) in SHORTCUT_BANK1_BITS.iter().enumerate() {
            if b0 & (1 << (10 + i)) != 0 {
                pending.push(32 + bit as usize);
            }
        }
    }
    // Shortcut aliases: bank 0 bits 15-20 -> bank 2 bits {21..25, 30}.
    if b0 & SHORTCUT_MASK2 != 0 {
        for (i, &bit) in SHORTCUT_BANK2_BITS.iter().enumerate() {
            if b0 & (1 << (15 + i)) != 0 {
                pending.push(64 + bit as usize);
            }
        }
    }
    // Overflow bits: bank 1 (bit 8) and bank 2 (bit 9).
    if b0 & (1 << 8) != 0 {
        let bank = mmio_read(armc_addr(ARMC_BANK1_PENDING));
        for n in 0..32 {
            if bank & (1 << n) != 0 {
                pending.push(32 + n);
            }
        }
    }
    if b0 & (1 << 9) != 0 {
        let bank = mmio_read(armc_addr(ARMC_BANK2_PENDING));
        for n in 0..32 {
            if bank & (1 << n) != 0 {
                pending.push(64 + n);
            }
        }
    }
    pending
}

/// Dispatches all pending peripheral IRQs through the dynamic IRQ framework.
#[cfg(all(not(feature = "legacy"), feature = "irq"))]
fn dispatch_armctrl() {
    for raw in scan_armctrl_pending() {
        if let Ok(irq) = legacy_irq(raw) {
            let _ = dispatch_irq(irq);
        }
    }
}

#[cfg(all(not(feature = "legacy"), feature = "irq"))]
struct IrqIfImpl;

#[cfg(all(not(feature = "legacy"), feature = "irq"))]
#[impl_plat_interface]
impl IrqIf for IrqIfImpl {
    fn prepare(_vector: TrapVector) {}

    fn init_boot_irqs(_cpu_id: usize) -> Result<(), IrqError> {
        init_boot_irqs_result()
    }

    fn set_enable(irq: IrqId, enabled: bool) -> Result<(), IrqError> {
        if irq.domain == CPU_LOCAL_IRQ_DOMAIN {
            if irq.hwirq.0 == LOCAL_IRQ_CNTPNSIRQ {
                let reg = local_addr(LOCAL_TIMER_INT_CONTROL0);
                let value = mmio_read(reg);
                if enabled {
                    mmio_write(reg, value | (1 << LOCAL_IRQ_CNTPNSIRQ));
                } else {
                    mmio_write(reg, value & !(1 << LOCAL_IRQ_CNTPNSIRQ));
                }
                return Ok(());
            }
            return Err(IrqError::Unsupported);
        }
        let raw = legacy_irq_raw(irq).ok_or(IrqError::Unsupported)?;
        if set_enable_shared(raw, enabled) {
            Ok(())
        } else {
            Err(IrqError::InvalidIrq)
        }
    }

    fn set_affinity(_irq: IrqId, _affinity: IrqAffinity) -> Result<(), IrqError> {
        Err(IrqError::Unsupported)
    }

    fn handle(_vector: TrapVector) -> Option<IrqId> {
        let pending = mmio_read(local_addr(LOCAL_IRQ_PENDING0));
        let mut first: Option<IrqId> = None;
        // The banked controller is chained on the GPU interrupt (local IRQ 8).
        if pending & (1 << LOCAL_IRQ_GPU_FAST) != 0 {
            first = first.or_else(|| legacy_irq(0).ok());
            dispatch_armctrl();
        }
        if pending & (1 << LOCAL_IRQ_CNTPNSIRQ) != 0 {
            first.get_or_insert(timer_irq());
            let _ = dispatch_irq(timer_irq());
        }
        first
    }

    fn send_ipi(irq_num: IrqId, target: IpiTarget) {
        let raw = irq_num.hwirq.0;
        match target {
            IpiTarget::Current { cpu_id } => {
                let reg = local_addr(LOCAL_MAILBOX0_SET0 + 16 * cpu_id);
                mmio_write(reg, 1 << raw);
            }
            IpiTarget::Other { cpu_id } | IpiTarget::AllExceptCurrent { cpu_id, .. } => {
                let reg = local_addr(LOCAL_MAILBOX0_SET0 + 16 * cpu_id);
                mmio_write(reg, 1 << raw);
            }
        }
    }

    fn ipi_irq() -> IrqId {
        // Single-CPU platform: reuse the timer id in the CPU-local domain;
        // no IPI is ever delivered.
        timer_irq()
    }

    fn resolve_source(_source: IrqSource) -> Result<IrqId, IrqError> {
        Err(IrqError::Unsupported)
    }

    fn resolve_percpu(hwirq: HwIrq) -> Result<IrqId, IrqError> {
        if hwirq.0 == LOCAL_IRQ_CNTPNSIRQ {
            Ok(timer_irq())
        } else {
            Err(IrqError::Unsupported)
        }
    }
}
