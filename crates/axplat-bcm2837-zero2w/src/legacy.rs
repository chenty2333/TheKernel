//! Legacy `axplat` (crates.io 0.3.x) interface implementations, used by
//! TheKernel. The same BCM2837 hardware code backs both interfaces; only the
//! interface adaptation differs.
//!
//! IRQ numbering for the legacy interface: peripheral IRQs use the armctrl
//! numbers 0..=95; the CPU-local timer interrupt is reported as the sentinel
//! [`TIMER_IRQ_LEGACY`] (96), which is outside the peripheral range.

use core::sync::atomic::{AtomicPtr, Ordering};

use axcpu_old;
use axplat_old::{
    console::ConsoleIf,
    impl_plat_interface,
    init::InitIf,
    irq::{HandlerTable, IpiTarget, IrqHandler, IrqIf},
    mem::{MemIf, PhysAddr, RawRange, VirtAddr, pa, va},
    power::PowerIf,
    time::TimeIf,
};

use crate::{
    config::{
        KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE, LOCAL_IRQ_CNTPNSIRQ, LOCAL_IRQ_GPU_FAST,
        LOCAL_IRQ_PENDING0, PHYS_VIRT_OFFSET,
    },
    irq::{local_addr, mmio_read, scan_armctrl_pending, timer_irq_legacy_enable},
};

/// Sentinel IRQ number for the CPU-local timer in the legacy interface.
pub const TIMER_IRQ_LEGACY: usize = 96;

const MAX_PERIPHERAL_IRQ_COUNT: usize = 96;

static TIMER_HANDLER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

static IRQ_HANDLER_TABLE: HandlerTable<MAX_PERIPHERAL_IRQ_COUNT> = HandlerTable::new();

// ---------------------------------------------------------------------------
// Console
// ---------------------------------------------------------------------------

struct LegacyConsole;

#[impl_plat_interface]
impl ConsoleIf for LegacyConsole {
    fn write_bytes(bytes: &[u8]) {
        crate::console::write_bytes(bytes);
    }

    fn read_bytes(bytes: &mut [u8]) -> usize {
        crate::console::read_bytes(bytes)
    }

    fn irq_num() -> Option<usize> {
        Some(crate::config::UART0_IRQ)
    }
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

struct LegacyTime;

#[impl_plat_interface]
impl TimeIf for LegacyTime {
    fn current_ticks() -> u64 {
        crate::time::current_ticks_shared()
    }

    fn ticks_to_nanos(ticks: u64) -> u64 {
        crate::time::ticks_to_nanos_shared(ticks)
    }

    fn nanos_to_ticks(nanos: u64) -> u64 {
        crate::time::nanos_to_ticks_shared(nanos)
    }

    fn epochoffset_nanos() -> u64 {
        crate::time::epochoffset_nanos_shared()
    }

    fn irq_num() -> usize {
        TIMER_IRQ_LEGACY
    }

    fn set_oneshot_timer(deadline_ns: u64) {
        crate::time::set_oneshot_timer_shared(deadline_ns);
    }
}

// ---------------------------------------------------------------------------
// Power
// ---------------------------------------------------------------------------

struct LegacyPower;

#[impl_plat_interface]
impl PowerIf for LegacyPower {
    /// Bootstraps the given CPU core with the given initial stack (physical).
    #[cfg(feature = "smp")]
    fn cpu_boot(cpu_id: usize, stack_top_paddr: usize) {
        crate::power::cpu_boot_shared(cpu_id, stack_top_paddr);
    }

    fn system_off() -> ! {
        crate::power::system_off()
    }

    fn cpu_num() -> usize {
        crate::config::MAX_CPU_NUM
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

struct LegacyMem;

#[impl_plat_interface]
impl MemIf for LegacyMem {
    fn phys_ram_ranges() -> &'static [RawRange] {
        crate::mem::legacy_phys_ram_ranges()
    }

    fn reserved_phys_ram_ranges() -> &'static [RawRange] {
        crate::mem::legacy_reserved_phys_ram_ranges()
    }

    fn mmio_ranges() -> &'static [RawRange] {
        crate::mem::legacy_mmio_ranges()
    }

    fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
        va!(paddr.as_usize() + PHYS_VIRT_OFFSET)
    }

    fn virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
        pa!(vaddr.as_usize() - PHYS_VIRT_OFFSET)
    }

    fn kernel_aspace() -> (VirtAddr, usize) {
        (va!(KERNEL_ASPACE_BASE), KERNEL_ASPACE_SIZE)
    }
}

// ---------------------------------------------------------------------------
// IRQ
// ---------------------------------------------------------------------------

fn handle_timer() -> Option<usize> {
    let handler = TIMER_HANDLER.load(Ordering::Acquire);
    if !handler.is_null() {
        // SAFETY: only function pointers are stored in TIMER_HANDLER.
        unsafe { core::mem::transmute::<*mut (), IrqHandler>(handler)() };
    }
    Some(TIMER_IRQ_LEGACY)
}

struct LegacyIrq;

#[impl_plat_interface]
impl IrqIf for LegacyIrq {
    fn set_enable(irq: usize, enabled: bool) {
        if irq == TIMER_IRQ_LEGACY {
            timer_irq_legacy_enable(enabled);
        } else if irq < MAX_PERIPHERAL_IRQ_COUNT {
            let _ = crate::irq::set_enable_shared(irq, enabled);
        }
    }

    fn register(irq: usize, handler: IrqHandler) -> bool {
        if irq == TIMER_IRQ_LEGACY {
            TIMER_HANDLER
                .compare_exchange(
                    core::ptr::null_mut(),
                    handler as *mut _,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        } else if irq < MAX_PERIPHERAL_IRQ_COUNT {
            if IRQ_HANDLER_TABLE.register_handler(irq, handler) {
                Self::set_enable(irq, true);
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    fn unregister(irq: usize) -> Option<IrqHandler> {
        if irq == TIMER_IRQ_LEGACY {
            let handler = TIMER_HANDLER.swap(core::ptr::null_mut(), Ordering::AcqRel);
            if handler.is_null() {
                None
            } else {
                // SAFETY: only function pointers are stored in TIMER_HANDLER.
                Some(unsafe { core::mem::transmute::<*mut (), IrqHandler>(handler) })
            }
        } else if irq < MAX_PERIPHERAL_IRQ_COUNT {
            IRQ_HANDLER_TABLE
                .unregister_handler(irq)
                .inspect(|_| Self::set_enable(irq, false))
        } else {
            None
        }
    }

    fn handle(_irq: usize) -> Option<usize> {
        let pending = mmio_read(local_addr(LOCAL_IRQ_PENDING0));
        if pending & (1 << LOCAL_IRQ_GPU_FAST) != 0 {
            let first = scan_armctrl_pending();
            for raw in &first {
                IRQ_HANDLER_TABLE.handle(*raw);
            }
            return first.first().copied();
        }
        if pending & (1 << LOCAL_IRQ_CNTPNSIRQ) != 0 {
            return handle_timer();
        }
        None
    }

    fn send_ipi(_irq_num: usize, _target: IpiTarget) {
        // Single-CPU platform; IPIs are never delivered.
    }
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

struct LegacyInit;

#[impl_plat_interface]
impl InitIf for LegacyInit {
    fn init_early(_cpu_id: usize, _arg: usize) {
        axcpu_old::init::init_trap();
        crate::console::init_early();
        crate::time::init_early();
    }

    #[cfg(feature = "smp")]
    fn init_early_secondary(_cpu_id: usize) {
        axcpu_old::init::init_trap();
        crate::time::init_early();
    }

    fn init_later(_cpu_id: usize, _arg: usize) {
        crate::irq::init_boot_irqs_shared();
    }

    #[cfg(feature = "smp")]
    fn init_later_secondary(_cpu_id: usize) {
        crate::irq::init_boot_irqs_shared();
    }
}
