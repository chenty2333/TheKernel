//! Power control for the BCM2837.
//!
//! The BCM2835/6/7 power management block (`0x3f10_0000`) accepts writes only
//! when the top byte carries the 0x5a password. The SoC has no true power
//! down; `system_off` falls back to a watchdog reboot, which is the
//! conventional bare-metal behaviour on these boards.

#[cfg(not(feature = "legacy"))]
#[cfg(not(feature = "legacy"))]
use ax_plat::power::PowerIf;

use crate::{
    config::*,
    irq::{mmio_read, mmio_write},
};

const PM_PASSWORD_MASK: u32 = 0xffff_ff00;

const fn pm_addr(offset: usize) -> usize {
    PM_PADDR + PHYS_VIRT_OFFSET + offset
}

/// Issues a watchdog reboot: arm the watchdog with a short timeout, then
/// request a full reset.
fn watchdog_reset() {
    mmio_write(pm_addr(PM_WDOG), PM_PASSWORD | 1);
    mmio_write(
        pm_addr(PM_RSTC),
        (mmio_read(pm_addr(PM_RSTC)) & PM_PASSWORD_MASK) | PM_RSTC_WRCFG_FULL_RESET,
    );
}

fn halt() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// Shuts the system down (shared with the legacy interface).
pub(crate) fn system_off() -> ! {
    watchdog_reset();
    halt()
}

/// Bootstraps a secondary CPU through the BCM2836 local mailbox: stores the
/// per-CPU stack top in the boot parameters, then writes the secondary entry
/// physical address to the core's mailbox, releasing it from reset.
#[cfg(feature = "smp")]
pub(crate) fn cpu_boot_shared(cpu_id: usize, stack_top_paddr: usize) {
    assert!(cpu_id < MAX_CPU_NUM, "secondary CPU index out of range");
    crate::boot::secondary_stack_store(cpu_id, stack_top_paddr);
    let entry = crate::boot::secondary_entry_paddr() as u32;
    // SAFETY: the write releases the core; the boot parameters were stored
    // and made visible by the store above (release ordering via the mailbox
    // write is provided by the MMIO access).
    mmio_write(
        LOCAL_INTC_PADDR + PHYS_VIRT_OFFSET + LOCAL_MAILBOX0_SET0 + 16 * cpu_id,
        entry,
    );
}

#[cfg(not(feature = "legacy"))]
struct PowerImpl;

#[cfg(not(feature = "legacy"))]
#[impl_plat_interface]
impl PowerIf for PowerImpl {
    /// Bootstraps the given CPU core with the given initial stack (physical).
    #[cfg(feature = "smp")]
    fn cpu_boot(cpu_id: usize, stack_top_paddr: usize) {
        cpu_boot_shared(cpu_id, stack_top_paddr);
    }

    fn system_off() -> ! {
        system_off()
    }

    fn system_reset() -> ! {
        watchdog_reset();
        halt()
    }

    fn cpu_num() -> usize {
        MAX_CPU_NUM
    }
}
