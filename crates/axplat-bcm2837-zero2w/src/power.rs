//! Power control for the BCM2837.
//!
//! The BCM2835/6/7 power management block (`0x3f10_0000`) accepts writes only
//! when the top byte carries the 0x5a password. The SoC has no true power
//! down; `system_off` falls back to a watchdog reboot, which is the
//! conventional bare-metal behaviour on these boards.

#[cfg(not(feature = "legacy"))]
use ax_plat::power::PowerIf;

use crate::config::*;
use crate::irq::{mmio_read, mmio_write};

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

#[cfg(not(feature = "legacy"))]
struct PowerImpl;

#[cfg(not(feature = "legacy"))]
#[impl_plat_interface]
impl PowerIf for PowerImpl {
    /// Bootstraps the given CPU core with the given initial stack.
    ///
    /// Not supported yet: the platform currently boots a single CPU core.
    #[cfg(feature = "smp")]
    fn cpu_boot(_cpu_id: usize, _stack_top_paddr: usize) {
        unimplemented!("BCM2837 secondary-core boot (mailbox) is not implemented yet");
    }

    fn system_off() -> ! {
        system_off()
    }

    fn system_reset() -> ! {
        watchdog_reset();
        halt()
    }

    fn cpu_num() -> usize {
        1
    }
}
