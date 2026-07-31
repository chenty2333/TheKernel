//! ARM generic timer on the Cortex-A53 (BCM2837).
//!
//! The CPU-local interrupt controller (BCM2836) routes the physical
//! non-secure timer interrupt (CNTPNSIRQ, local IRQ 1) to the CPU when
//! `LOCAL_TIMER_INT_CONTROL0` bit 1 is set. The timer frequency is read from
//! `CNTFRQ_EL0`, which the GPU firmware programs at boot.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(feature = "legacy"))]
use ax_plat::time::{NANOS_PER_SEC, TimeIf};
#[cfg(feature = "legacy")]
use axplat_old::time::NANOS_PER_SEC;

#[cfg(not(feature = "legacy"))]
use crate::config::LOCAL_IRQ_CNTPNSIRQ;

static TIMER_FREQ: AtomicU64 = AtomicU64::new(0);
static EPOCH_OFFSET_NANOS: AtomicU64 = AtomicU64::new(0);

/// Reads the current hardware tick count (`CNTPCT_EL0`).
pub(crate) fn current_ticks_shared() -> u64 {
    let ticks: u64;
    unsafe {
        asm!("mrs {0}, cntpct_el0", out(reg) ticks, options(nomem, nostack, preserves_flags));
    }
    ticks
}

fn freq() -> u64 {
    let cached = TIMER_FREQ.load(Ordering::Acquire);
    if cached != 0 {
        return cached;
    }
    let mut f: u64;
    unsafe {
        asm!("mrs {0}, cntfrq_el0", out(reg) f, options(nomem, nostack, preserves_flags));
    }
    if f == 0 {
        // Fallback: the standard BCM2837 firmware value.
        f = 19_200_000;
    }
    TIMER_FREQ.store(f, Ordering::Release);
    f
}

pub(crate) fn ticks_to_nanos_shared(ticks: u64) -> u64 {
    let f = freq();
    if f == 0 {
        return 0;
    }
    ((ticks as u128 * NANOS_PER_SEC as u128) / f as u128) as u64
}

pub(crate) fn nanos_to_ticks_shared(nanos: u64) -> u64 {
    let f = freq();
    if f == 0 {
        return 0;
    }
    ((nanos as u128 * f as u128) / NANOS_PER_SEC as u128) as u64
}

pub(crate) fn epochoffset_nanos_shared() -> u64 {
    EPOCH_OFFSET_NANOS.load(Ordering::Acquire)
}

pub(crate) fn set_oneshot_timer_shared(deadline_ns: u64) {
    let now = current_ticks_shared();
    let deadline = nanos_to_ticks_shared(deadline_ns);
    let interval = if now < deadline {
        let interval = deadline - now;
        debug_assert!(interval <= u32::MAX as u64, "timer interval too large");
        interval
    } else {
        0
    };
    unsafe {
        // CNTP_TVAL_EL0: trigger after `interval` ticks; writing the compare
        // value also clears the interrupt condition.
        asm!(
            "msr cntp_tval_el0, {0}",
            in(reg) interval,
            options(nomem, nostack, preserves_flags),
        );
        // CNTP_CTL_EL0: enable the timer and unmask the interrupt.
        asm!(
            "msr cntp_ctl_el0, {0}",
            in(reg) 0b01u64,
            options(nomem, nostack, preserves_flags),
        );
    }
}

pub(crate) fn init_early() {
    let _ = freq();
}

#[cfg(not(feature = "legacy"))]
struct TimeIfImpl;

#[cfg(not(feature = "legacy"))]
#[impl_plat_interface]
impl TimeIf for TimeIfImpl {
    fn current_ticks() -> u64 {
        current_ticks_shared()
    }

    fn ticks_to_nanos(ticks: u64) -> u64 {
        ticks_to_nanos_shared(ticks)
    }

    fn nanos_to_ticks(nanos: u64) -> u64 {
        nanos_to_ticks_shared(nanos)
    }

    fn epochoffset_nanos() -> u64 {
        epochoffset_nanos_shared()
    }

    #[cfg(feature = "irq")]
    fn irq_num() -> ax_plat::irq::IrqId {
        ax_plat::irq::IrqId::new(
            ax_plat::irq::CPU_LOCAL_IRQ_DOMAIN,
            ax_plat::irq::HwIrq(LOCAL_IRQ_CNTPNSIRQ),
        )
    }

    #[cfg(feature = "irq")]
    fn set_oneshot_timer(deadline_ns: u64) {
        set_oneshot_timer_shared(deadline_ns);
    }
}
