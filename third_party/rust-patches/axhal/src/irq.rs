//! Interrupt management.

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

#[cfg(feature = "ipi")]
pub use axconfig::devices::IPI_IRQ;
use axcpu::trap::{IRQ, IrqBoundary, register_trap_handler};
#[cfg(feature = "ipi")]
pub use axplat::irq::{IpiTarget, send_ipi};
pub use axplat::irq::{handle, register, set_enable, unregister};
use percpu::def_percpu;

static IRQ_HOOK: AtomicUsize = AtomicUsize::new(0);

const IRQ_BOUNDARY_UNINITIALIZED: u8 = 0;
const IRQ_BOUNDARY_INSTALLED: u8 = 1;
const IRQ_BOUNDARY_CONFLICT: u8 = 2;

static IRQ_BOUNDARY_STATE: AtomicU8 = AtomicU8::new(IRQ_BOUNDARY_UNINITIALIZED);
static IRQ_EXIT_HOOK: AtomicUsize = AtomicUsize::new(0);

#[def_percpu]
static IRQ_DEPTH: usize = 0;

#[def_percpu]
static IRQ_EXIT_PHASE: bool = false;

#[inline]
fn enter_irq_depth(depth: &mut usize) {
    *depth = depth.checked_add(1).expect("IRQ nesting depth overflow");
}

#[inline]
fn leave_irq_depth(depth: &mut usize) -> bool {
    let next = depth.checked_sub(1).expect("IRQ exit without enter");
    *depth = next;
    next == 0
}

fn irq_boundary(boundary: IrqBoundary) {
    match boundary {
        IrqBoundary::Enter => {
            let depth = unsafe { IRQ_DEPTH.current_ref_mut_raw() };
            enter_irq_depth(depth);
        }
        IrqBoundary::Exit => {
            let next = {
                let depth = unsafe { IRQ_DEPTH.current_ref_mut_raw() };
                leave_irq_depth(depth)
            };
            if next {
                // Do not keep a mutable reference across the callback: the
                // scheduler may context-switch while consuming the hook.
                unsafe { *IRQ_EXIT_PHASE.current_ref_mut_raw() = true };
                let hook = IRQ_EXIT_HOOK.load(Ordering::Acquire);
                if hook != 0 {
                    // SAFETY: the slot only accepts a function pointer and is
                    // never cleared while the kernel is running.
                    let hook = unsafe { core::mem::transmute::<usize, fn()>(hook) };
                    hook();
                }
                unsafe { *IRQ_EXIT_PHASE.current_ref_mut_raw() = false };
            }
        }
    }
}

fn ensure_irq_boundary_hook() -> bool {
    match IRQ_BOUNDARY_STATE.load(Ordering::Acquire) {
        IRQ_BOUNDARY_INSTALLED => true,
        IRQ_BOUNDARY_CONFLICT => false,
        _ => {
            let installed = axcpu::trap::register_irq_boundary_hook(irq_boundary);
            let state = if installed {
                IRQ_BOUNDARY_INSTALLED
            } else {
                IRQ_BOUNDARY_CONFLICT
            };
            IRQ_BOUNDARY_STATE.store(state, Ordering::Release);
            installed
        }
    }
}

/// Registers the callback consumed at the outermost IRQ return boundary.
///
/// The callback runs after the platform IRQ handler's `NoPreempt` guard has
/// been released, while the architecture still keeps local interrupts
/// masked. Registration is idempotent for the same function pointer and
/// rejects a different owner.
#[must_use]
pub fn register_irq_exit_hook(hook: fn()) -> bool {
    if !ensure_irq_boundary_hook() {
        return false;
    }
    let address = hook as usize;
    match IRQ_EXIT_HOOK.compare_exchange(0, address, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => true,
        Err(existing) => existing == address,
    }
}

/// Returns whether the current CPU is inside a hardware IRQ dispatch.
///
/// Callers must already hold a preemption-disabled or IRQ-disabled guard so
/// the current CPU cannot migrate while the per-CPU value is read.
#[inline]
pub fn in_irq_context() -> bool {
    unsafe { *IRQ_DEPTH.current_ref_raw() != 0 || *IRQ_EXIT_PHASE.current_ref_raw() }
}

/// Returns whether the current CPU is running the outermost IRQ-exit hook.
///
/// This is a narrower state than [`in_irq_context`]. The scheduler uses it to
/// allow its one exit-phase check while still suppressing recursive
/// rescheduling from guards dropped inside that check.
#[inline]
pub fn in_irq_exit_phase() -> bool {
    unsafe { *IRQ_EXIT_PHASE.current_ref_raw() }
}

/// Register a hook function called after an IRQ is handled.
///
/// This function can be called only once; subsequent calls will return false.
///
/// TODO: design a better api!
pub fn register_irq_hook(hook: fn(usize)) -> bool {
    IRQ_HOOK
        .compare_exchange(
            0,
            hook as *const () as usize,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
}

/// IRQ handler.
///
/// # Warn
///
/// Make sure called in an interrupt context or hypervisor VM exit handler.
#[register_trap_handler(IRQ)]
pub fn irq_handler(vector: usize) -> bool {
    let guard = kernel_guard::NoPreempt::new();

    if let Some(irq) = handle(vector) {
        let hook = IRQ_HOOK.load(Ordering::SeqCst);
        if hook != 0 {
            let hook = unsafe { core::mem::transmute::<usize, fn(usize)>(hook) };
            hook(irq);
        }
    }

    drop(guard); // rescheduling may occur when preemption is re-enabled.
    true
}

#[cfg(test)]
mod tests {
    #[inline(never)]
    fn first_exit_hook() {}

    #[inline(never)]
    fn second_exit_hook() {}

    #[test]
    fn exit_hook_has_one_stable_owner() {
        assert!(super::register_irq_exit_hook(first_exit_hook));
        assert!(super::register_irq_exit_hook(first_exit_hook));
        assert!(!super::register_irq_exit_hook(second_exit_hook));
    }

    #[test]
    fn nested_irq_depth_only_exits_at_zero() {
        let mut depth = 0;
        super::enter_irq_depth(&mut depth);
        super::enter_irq_depth(&mut depth);
        assert_eq!(depth, 2);
        assert!(!super::leave_irq_depth(&mut depth));
        assert!(super::leave_irq_depth(&mut depth));
        assert_eq!(depth, 0);
    }

    #[test]
    #[should_panic(expected = "IRQ nesting depth overflow")]
    fn irq_depth_overflow_is_fail_stop() {
        let mut depth = usize::MAX;
        super::enter_irq_depth(&mut depth);
    }

    #[test]
    #[should_panic(expected = "IRQ exit without enter")]
    fn irq_depth_underflow_is_fail_stop() {
        let mut depth = 0;
        super::leave_irq_depth(&mut depth);
    }
}
