//! Trap handling.

use core::sync::atomic::{AtomicUsize, Ordering};

pub use linkme::{
    distributed_slice as def_trap_handler, distributed_slice as register_trap_handler,
};
use memory_addr::VirtAddr;
pub use page_table_entry::MappingFlags as PageFaultFlags;

pub use crate::TrapFrame;

/// A slice of IRQ handler functions.
#[def_trap_handler]
pub static IRQ: [fn(usize) -> bool];

/// A slice of page fault handler functions.
#[def_trap_handler]
pub static PAGE_FAULT: [fn(VirtAddr, PageFaultFlags) -> bool];

/// The phase of a hardware interrupt dispatch.
///
/// The callback is invoked at the single Layer 0 boundary around the
/// registered IRQ handler. Consumers can use `Enter`/`Exit` to maintain
/// per-CPU interrupt nesting without guessing from the interrupt-enable bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqBoundary {
    /// The CPU is entering an IRQ handler.
    Enter,
    /// The registered IRQ handler has returned.
    Exit,
}

static IRQ_BOUNDARY_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Registers the single IRQ-boundary callback.
///
/// Registration is idempotent for the same function pointer and rejects a
/// different owner. The slot is fixed-capacity and never allocates, so the
/// callback remains valid on the interrupt hot path.
#[must_use]
pub fn register_irq_boundary_hook(hook: fn(IrqBoundary)) -> bool {
    let address = hook as usize;
    match IRQ_BOUNDARY_HOOK.compare_exchange(0, address, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => true,
        Err(existing) => existing == address,
    }
}

#[inline]
pub(crate) fn notify_irq_boundary(boundary: IrqBoundary) {
    let address = IRQ_BOUNDARY_HOOK.load(Ordering::Acquire);
    if address == 0 {
        return;
    }

    // SAFETY: the only value ever published to this slot is a function
    // pointer accepted by `register_irq_boundary_hook`, and the slot is never
    // cleared while the kernel is running.
    let hook = unsafe { core::mem::transmute::<usize, fn(IrqBoundary)>(address) };
    hook(boundary);
}

#[allow(unused_macros)]
macro_rules! handle_trap {
    (IRQ, $($args:tt)*) => {{
        $crate::trap::notify_irq_boundary($crate::trap::IrqBoundary::Enter);
        let result = {
            let mut iter = $crate::trap::IRQ.iter();
            if let Some(func) = iter.next() {
                if iter.next().is_some() {
                    warn!("Multiple handlers for trap IRQ are not currently supported");
                }
                func($($args)*)
            } else {
                warn!("No registered handler for trap IRQ");
                false
            }
        };
        $crate::trap::notify_irq_boundary($crate::trap::IrqBoundary::Exit);
        result
    }};
    ($trap:ident, $($args:tt)*) => {{
        let mut iter = $crate::trap::$trap.iter();
        if let Some(func) = iter.next() {
            if iter.next().is_some() {
                warn!("Multiple handlers for trap {} are not currently supported", stringify!($trap));
            }
            func($($args)*)
        } else {
            warn!("No registered handler for trap {}", stringify!($trap));
            false
        }
    }}
}
