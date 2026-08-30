//! Interrupt management.

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

#[cfg(feature = "ipi")]
use axconfig::devices::IPI_IRQ;
use axcpu::{
    TrapFrame,
    trap::{IRQ, IrqBoundary, register_trap_handler},
};
#[cfg(feature = "ipi")]
pub use axplat::irq::IpiTarget;
use axplat::irq::handle;
use percpu::def_percpu;

static IRQ_HOOK: AtomicUsize = AtomicUsize::new(0);
const CONTEXT_INSTALLING: usize = 1;
static IRQ_CONTEXT: [AtomicUsize; 256] = [const { AtomicUsize::new(0) }; 256];

const IRQ_BOUNDARY_UNINITIALIZED: u8 = 0;
const IRQ_BOUNDARY_INSTALLED: u8 = 1;
const IRQ_BOUNDARY_CONFLICT: u8 = 2;

static IRQ_BOUNDARY_STATE: AtomicU8 = AtomicU8::new(IRQ_BOUNDARY_UNINITIALIZED);
static IRQ_EXIT_HOOK: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "ipi")]
const IPI_BROKER_UNINITIALIZED: u8 = 0;
#[cfg(feature = "ipi")]
const IPI_BROKER_INSTALLING: u8 = 1;
#[cfg(feature = "ipi")]
const IPI_BROKER_INSTALLED: u8 = 2;
#[cfg(feature = "ipi")]
const IPI_BROKER_CONFLICT: u8 = 3;

#[cfg(feature = "ipi")]
static IPI_BROKER_STATE: AtomicU8 = AtomicU8::new(IPI_BROKER_UNINITIALIZED);
#[cfg(feature = "ipi")]
static IPI_BROKER_CPU_COUNT: AtomicUsize = AtomicUsize::new(0);

/// A rejected one-time initialization of the raw IPI broker.
#[cfg(feature = "ipi")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpiBrokerInitError {
    /// The admitted static topology is empty or exceeds the mailbox array.
    InvalidCpuCount {
        /// Rejected number of CPUs.
        requested: usize,
        /// Maximum number of CPU mailboxes compiled into the kernel.
        maximum: usize,
    },
    /// Another CPU is concurrently installing the raw vector.
    ConcurrentInitialization,
    /// Another raw interrupt handler already owns the platform vector.
    RawVectorConflict,
    /// A later caller tried to replace the already admitted static topology.
    TopologyConflict {
        /// CPU count already owned by the broker.
        admitted: usize,
        /// CPU count requested by the later caller.
        requested: usize,
    },
}

/// A fixed, coalescible reason carried by the single raw IPI vector.
#[cfg(feature = "ipi")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum IpiReason {
    /// Complete pending TLB or instruction-cache maintenance.
    CpuMaintenance = 0,
    /// Re-evaluate scheduling on the destination CPU.
    Reschedule     = 1,
    /// Reserved for a bounded cross-CPU call-function consumer.
    CallFunction   = 2,
    /// Acknowledge one private expedited membarrier generation.
    Membarrier     = 3,
    /// Stop a remote CPU for the terminal x86_64 kexec transition.
    ///
    /// This lane is deliberately separate from maintenance and scheduler
    /// work: its consumer never returns to ordinary execution after it has
    /// acknowledged the handoff generation.
    KexecStop      = 4,
    DeferredWork   = 5,
}

#[cfg(feature = "ipi")]
impl IpiReason {
    const ALL: [Self; 6] = [
        Self::CpuMaintenance,
        Self::Reschedule,
        Self::CallFunction,
        Self::Membarrier,
        Self::KexecStop,
        Self::DeferredWork,
    ];

    #[inline]
    const fn index(self) -> usize {
        self as usize
    }

    #[inline]
    const fn bit(self) -> u8 {
        1 << self.index()
    }
}

#[cfg(feature = "ipi")]
fn visit_pending_reasons(pending: u8, mut visit: impl FnMut(IpiReason)) -> Result<(), u8> {
    let known_mask = IpiReason::ALL
        .iter()
        .fold(0, |mask, reason| mask | reason.bit());
    let unknown = pending & !known_mask;
    if unknown != 0 {
        return Err(unknown);
    }
    for reason in IpiReason::ALL {
        if pending & reason.bit() != 0 {
            visit(reason);
        }
    }
    Ok(())
}

/// A rejected IPI reason publication.
#[cfg(feature = "ipi")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpiReasonError {
    /// The raw broker has not admitted a static CPU topology yet.
    BrokerNotInitialized,
    /// The selected reason has no registered consumer.
    UnregisteredReason(IpiReason),
    /// A target CPU is outside the admitted static topology.
    InvalidTarget {
        /// Rejected CPU identifier.
        cpu_id: usize,
        /// Number of CPUs admitted to the broker.
        cpu_num: usize,
    },
    /// The multicast topology differs from the admitted static CPU count.
    InvalidCpuCount {
        /// Rejected target count.
        requested: usize,
        /// Number of CPUs admitted to the broker.
        admitted: usize,
    },
    /// A target that names the current CPU supplied the wrong identity.
    IncorrectCurrentCpu {
        /// CPU identifier carried in the target.
        claimed: usize,
        /// CPU identifier observed by the broker.
        actual: usize,
    },
    /// An `Other` target incorrectly selected the publishing CPU itself.
    CurrentCpuIsNotRemote {
        /// Identifier of the publishing CPU.
        cpu_id: usize,
    },
}

#[cfg(feature = "ipi")]
struct IpiReasonHandlers {
    slots: [AtomicUsize; IpiReason::ALL.len()],
}

#[cfg(feature = "ipi")]
impl IpiReasonHandlers {
    const fn new() -> Self {
        Self {
            slots: [const { AtomicUsize::new(0) }; IpiReason::ALL.len()],
        }
    }

    fn register(&self, reason: IpiReason, handler: fn()) -> bool {
        let address = handler as usize;
        match self.slots[reason.index()].compare_exchange(
            0,
            address,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(existing) => existing == address,
        }
    }

    fn handler(&self, reason: IpiReason) -> usize {
        self.slots[reason.index()].load(Ordering::Acquire)
    }
}

#[cfg(feature = "ipi")]
static IPI_REASON_HANDLERS: IpiReasonHandlers = IpiReasonHandlers::new();

#[cfg(feature = "ipi")]
#[repr(align(64))]
struct IpiReasonMailbox {
    pending: AtomicU8,
}

#[cfg(feature = "ipi")]
impl IpiReasonMailbox {
    const fn new() -> Self {
        Self {
            pending: AtomicU8::new(0),
        }
    }

    fn publish(&self, reason: IpiReason) -> bool {
        self.pending.fetch_or(reason.bit(), Ordering::AcqRel) & reason.bit() == 0
    }

    fn take(&self) -> u8 {
        self.pending.swap(0, Ordering::AcqRel)
    }
}

#[cfg(feature = "ipi")]
static IPI_REASON_MAILBOXES: [IpiReasonMailbox; axconfig::plat::MAX_CPU_NUM] =
    [const { IpiReasonMailbox::new() }; axconfig::plat::MAX_CPU_NUM];

#[def_percpu]
static IRQ_DEPTH: usize = 0;

/// Enables or disables a device interrupt line.
///
/// The raw IPI vector is permanently managed by the typed broker.
pub fn set_enable(irq: usize, enabled: bool) {
    #[cfg(feature = "ipi")]
    if irq == IPI_IRQ {
        panic!("the raw IPI vector is managed by the typed broker");
    }
    axplat::irq::set_enable(irq, enabled);
}

/// Registers a device interrupt handler.
///
/// The raw IPI vector is owned by the fixed reason broker and must instead use
/// [`register_ipi_reason`].
#[must_use]
pub fn register(irq: usize, handler: axplat::irq::IrqHandler) -> bool {
    #[cfg(feature = "ipi")]
    if irq == IPI_IRQ {
        return false;
    }
    if irq >= IRQ_CONTEXT.len() || IRQ_CONTEXT[irq].load(Ordering::Acquire) == 0 {
        axplat::irq::register(irq, handler)
    } else {
        false
    }
}

/// Unregisters a device interrupt handler.
///
/// The raw IPI broker is permanent after initialization and cannot be removed
/// through the generic device-IRQ API.
pub fn unregister(irq: usize) -> Option<axplat::irq::IrqHandler> {
    #[cfg(feature = "ipi")]
    if irq == IPI_IRQ {
        return None;
    }
    if irq < IRQ_CONTEXT.len() && IRQ_CONTEXT[irq].load(Ordering::Acquire) != 0 {
        None
    } else {
        axplat::irq::unregister(irq)
    }
}

// `axplat::irq::IrqHandler` is a no-argument callback.  Dispatch has already
// retained the vector for EOI before this marker is invoked.
fn context_marker() {}

/// Registers a trap-frame-aware owner for one x86 hardware IRQ vector.
/// The matching no-op normal handler preserves platform dispatch and EOI.
#[must_use]
pub fn register_context(vector: usize, handler: fn(usize, &TrapFrame)) -> bool {
    if !ensure_irq_boundary_hook() {
        return false;
    }
    if vector >= IRQ_CONTEXT.len() || handler as usize == CONTEXT_INSTALLING {
        return false;
    }
    let slot = &IRQ_CONTEXT[vector];
    if slot
        .compare_exchange(0, CONTEXT_INSTALLING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return slot.load(Ordering::Acquire) == handler as usize;
    }
    if !axplat::irq::register(vector, context_marker) {
        slot.store(0, Ordering::Release);
        return false;
    }
    slot.store(handler as usize, Ordering::Release);
    true
}

/// Removes the context owner and its EOI-preserving normal marker.
pub fn unregister_context(vector: usize) -> Option<fn(usize, &TrapFrame)> {
    let slot = IRQ_CONTEXT.get(vector)?;
    let address = slot.load(Ordering::Acquire);
    if address == 0 || address == CONTEXT_INSTALLING {
        return None;
    }
    // Keep the slot occupied until the marker is gone: ordinary registration
    // observes it and cannot race into a later unregister.
    if axplat::irq::unregister(vector).is_none() {
        return None;
    }
    slot.store(0, Ordering::Release);
    // SAFETY: an installed slot contains only an accepted function pointer.
    Some(unsafe { core::mem::transmute::<usize, fn(usize, &TrapFrame)>(address) })
}

#[cfg(feature = "ipi")]
/// Installs the sole raw IPI handler and admits one immutable CPU topology.
///
/// Initialization is intended for the primary CPU before any IPI reason can
/// be published. Repeating the same topology is idempotent; concurrent setup
/// or an attempt to replace the topology is rejected without spinning.
pub fn init_ipi_broker(cpu_count: usize) -> Result<(), IpiBrokerInitError> {
    let maximum = IPI_REASON_MAILBOXES.len();
    if cpu_count == 0 || cpu_count > maximum {
        return Err(IpiBrokerInitError::InvalidCpuCount {
            requested: cpu_count,
            maximum,
        });
    }
    match IPI_BROKER_STATE.compare_exchange(
        IPI_BROKER_UNINITIALIZED,
        IPI_BROKER_INSTALLING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            if !axplat::irq::register(IPI_IRQ, ipi_broker_handler) {
                IPI_BROKER_STATE.store(IPI_BROKER_CONFLICT, Ordering::Release);
                return Err(IpiBrokerInitError::RawVectorConflict);
            }
            IPI_BROKER_CPU_COUNT.store(cpu_count, Ordering::Release);
            IPI_BROKER_STATE.store(IPI_BROKER_INSTALLED, Ordering::Release);
            Ok(())
        }
        Err(IPI_BROKER_INSTALLING) => Err(IpiBrokerInitError::ConcurrentInitialization),
        Err(IPI_BROKER_INSTALLED) => {
            let admitted = IPI_BROKER_CPU_COUNT.load(Ordering::Acquire);
            if admitted == cpu_count {
                Ok(())
            } else {
                Err(IpiBrokerInitError::TopologyConflict {
                    admitted,
                    requested: cpu_count,
                })
            }
        }
        Err(IPI_BROKER_CONFLICT) => Err(IpiBrokerInitError::RawVectorConflict),
        Err(_) => unreachable!(),
    }
}

/// Registers one stable consumer for an IPI reason lane.
///
/// Repeating the same registration is idempotent; a different consumer for an
/// occupied lane is rejected. Registered consumers are never cleared.
#[cfg(feature = "ipi")]
#[must_use]
pub fn register_ipi_reason(reason: IpiReason, handler: fn()) -> bool {
    IPI_BROKER_STATE.load(Ordering::Acquire) == IPI_BROKER_INSTALLED
        && IPI_REASON_HANDLERS.register(reason, handler)
}

#[cfg(feature = "ipi")]
fn visit_ipi_targets(
    target: &IpiTarget,
    admitted: usize,
    current_cpu: usize,
    mut visit: impl FnMut(usize),
) -> Result<(), IpiReasonError> {
    match target {
        IpiTarget::Current { cpu_id } => {
            let cpu_id = *cpu_id;
            if cpu_id >= admitted {
                return Err(IpiReasonError::InvalidTarget {
                    cpu_id,
                    cpu_num: admitted,
                });
            }
            if cpu_id != current_cpu {
                return Err(IpiReasonError::IncorrectCurrentCpu {
                    claimed: cpu_id,
                    actual: current_cpu,
                });
            }
            visit(cpu_id);
        }
        IpiTarget::Other { cpu_id } => {
            let cpu_id = *cpu_id;
            if cpu_id >= admitted {
                return Err(IpiReasonError::InvalidTarget {
                    cpu_id,
                    cpu_num: admitted,
                });
            }
            if cpu_id == current_cpu {
                return Err(IpiReasonError::CurrentCpuIsNotRemote { cpu_id });
            }
            visit(cpu_id);
        }
        IpiTarget::AllExceptCurrent { cpu_id, cpu_num } => {
            let cpu_id = *cpu_id;
            let cpu_num = *cpu_num;
            if cpu_num != admitted {
                return Err(IpiReasonError::InvalidCpuCount {
                    requested: cpu_num,
                    admitted,
                });
            }
            if cpu_id >= cpu_num {
                return Err(IpiReasonError::InvalidTarget { cpu_id, cpu_num });
            }
            if cpu_id != current_cpu {
                return Err(IpiReasonError::IncorrectCurrentCpu {
                    claimed: cpu_id,
                    actual: current_cpu,
                });
            }
            for target_cpu in 0..cpu_num {
                if target_cpu != cpu_id {
                    visit(target_cpu);
                }
            }
        }
    }
    Ok(())
}

/// Publishes a coalescible reason and sends the raw hardware IPI.
///
/// The hardware kick is intentionally not suppressed when the reason bit was
/// already pending: recovery users may re-kick a delayed target without
/// allocating a second work item.
#[cfg(feature = "ipi")]
pub fn send_ipi_reason(reason: IpiReason, target: IpiTarget) -> Result<(), IpiReasonError> {
    if IPI_BROKER_STATE.load(Ordering::Acquire) != IPI_BROKER_INSTALLED {
        return Err(IpiReasonError::BrokerNotInitialized);
    }
    if IPI_REASON_HANDLERS.handler(reason) == 0 {
        return Err(IpiReasonError::UnregisteredReason(reason));
    }
    // `IpiTarget` carries the publisher's CPU identity. Keep that identity
    // stable through validation, mailbox publication, and the hardware kick.
    let _guard = kernel_guard::NoPreempt::new();
    let admitted = IPI_BROKER_CPU_COUNT.load(Ordering::Acquire);
    let current_cpu = crate::percpu::this_cpu_id();
    visit_ipi_targets(&target, admitted, current_cpu, |cpu| {
        let _ = IPI_REASON_MAILBOXES[cpu].publish(reason);
    })?;
    axplat::irq::send_ipi(IPI_IRQ, target);
    Ok(())
}

#[cfg(feature = "ipi")]
fn ipi_broker_handler() {
    let cpu = crate::percpu::this_cpu_id();
    if IPI_BROKER_STATE.load(Ordering::Acquire) != IPI_BROKER_INSTALLED
        || cpu >= IPI_BROKER_CPU_COUNT.load(Ordering::Acquire)
    {
        crate::power::system_off();
    }
    let Some(mailbox) = IPI_REASON_MAILBOXES.get(cpu) else {
        crate::power::system_off();
    };
    let pending = mailbox.take();
    if visit_pending_reasons(pending, |reason| {
        let handler = IPI_REASON_HANDLERS.handler(reason);
        if handler == 0 {
            crate::power::system_off();
        }
        // SAFETY: registration only accepts a function pointer and registered
        // slots are never cleared or replaced by a different owner.
        let handler = unsafe { core::mem::transmute::<usize, fn()>(handler) };
        handler();
    })
    .is_err()
    {
        crate::power::system_off();
    }
}

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
                let hook = IRQ_EXIT_HOOK.load(Ordering::Acquire);
                if hook != 0 {
                    // SAFETY: the slot only accepts a function pointer and is
                    // never cleared while the kernel is running.
                    let hook = unsafe { core::mem::transmute::<usize, fn()>(hook) };
                    hook();
                }
            }
        }
    }
}

fn irq_context(boundary: IrqBoundary, vector: usize, frame: &TrapFrame) {
    if boundary != IrqBoundary::Enter {
        return;
    }
    let address = IRQ_CONTEXT[vector & 0xff].load(Ordering::Acquire);
    if address != 0 && address != CONTEXT_INSTALLING {
        // SAFETY: registration publishes an immutable function pointer.
        let handler = unsafe { core::mem::transmute::<usize, fn(usize, &TrapFrame)>(address) };
        handler(vector, frame);
    }
}

fn ensure_irq_boundary_hook() -> bool {
    match IRQ_BOUNDARY_STATE.load(Ordering::Acquire) {
        IRQ_BOUNDARY_INSTALLED => true,
        IRQ_BOUNDARY_CONFLICT => false,
        _ => {
            let installed = axcpu::trap::register_irq_boundary_hook(irq_boundary)
                && axcpu::trap::register_irq_context_hook(irq_context);
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

/// Returns whether the current CPU is inside a hardware IRQ handler.
///
/// The outermost exit hook runs after this depth reaches zero. It must not be
/// represented as IRQ context: the hook may switch tasks before it returns,
/// and a per-CPU phase bit would then leak into the newly scheduled task.
#[inline]
pub fn in_irq_context() -> bool {
    // The public accessor is safe, so it must stabilize the current CPU rather
    // than exporting `percpu::current_ref_raw`'s precondition to every caller.
    let _guard = kernel_guard::IrqSave::new();
    unsafe { *IRQ_DEPTH.current_ref_raw() != 0 }
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
    #[cfg(feature = "ipi")]
    use std::{sync::Arc, thread, vec::Vec};

    #[inline(never)]
    fn first_exit_hook() {}

    #[inline(never)]
    fn second_exit_hook() {}

    #[cfg(feature = "ipi")]
    #[inline(never)]
    fn first_reason_handler() {}

    #[cfg(feature = "ipi")]
    #[inline(never)]
    fn second_reason_handler() {}

    #[cfg(feature = "ipi")]
    #[inline(never)]
    fn raw_ipi_handler() {}

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

    #[cfg(feature = "ipi")]
    #[test]
    fn reason_mailbox_coalesces_and_takes_one_snapshot() {
        let mailbox = super::IpiReasonMailbox::new();
        assert!(mailbox.publish(super::IpiReason::CpuMaintenance));
        assert!(!mailbox.publish(super::IpiReason::CpuMaintenance));
        assert!(mailbox.publish(super::IpiReason::Reschedule));
        assert_eq!(
            mailbox.take(),
            super::IpiReason::CpuMaintenance.bit() | super::IpiReason::Reschedule.bit()
        );
        assert_eq!(mailbox.take(), 0);
    }

    #[cfg(feature = "ipi")]
    #[test]
    fn raw_ipi_vector_cannot_use_device_registration() {
        assert!(!super::register(super::IPI_IRQ, raw_ipi_handler));
        assert_eq!(super::unregister(super::IPI_IRQ), None);
    }

    #[cfg(feature = "ipi")]
    #[test]
    #[should_panic(expected = "raw IPI vector is managed by the typed broker")]
    fn raw_ipi_vector_cannot_use_device_enable_control() {
        super::set_enable(super::IPI_IRQ, false);
    }

    #[cfg(feature = "ipi")]
    #[test]
    fn broker_rejects_impossible_static_topologies_before_installation() {
        assert_eq!(
            super::init_ipi_broker(0),
            Err(super::IpiBrokerInitError::InvalidCpuCount {
                requested: 0,
                maximum: super::IPI_REASON_MAILBOXES.len(),
            })
        );
        let requested = super::IPI_REASON_MAILBOXES.len() + 1;
        assert_eq!(
            super::init_ipi_broker(requested),
            Err(super::IpiBrokerInitError::InvalidCpuCount {
                requested,
                maximum: super::IPI_REASON_MAILBOXES.len(),
            })
        );
    }

    #[cfg(feature = "ipi")]
    #[test]
    fn concurrent_reason_publication_preserves_every_reason_bit() {
        let mailbox = Arc::new(super::IpiReasonMailbox::new());
        let mut workers = Vec::new();
        for reason in super::IpiReason::ALL {
            let mailbox = Arc::clone(&mailbox);
            workers.push(thread::spawn(move || {
                for _ in 0..1_000 {
                    let _ = mailbox.publish(reason);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            mailbox.take(),
            super::IpiReason::ALL
                .iter()
                .fold(0, |mask, reason| mask | reason.bit())
        );
    }

    #[cfg(feature = "ipi")]
    #[test]
    fn reason_snapshot_dispatch_is_ordered_and_rejects_unknown_bits() {
        let mut visited = Vec::new();
        super::visit_pending_reasons(
            super::IpiReason::CallFunction.bit()
                | super::IpiReason::CpuMaintenance.bit()
                | super::IpiReason::Reschedule.bit()
                | super::IpiReason::Membarrier.bit(),
            |reason| visited.push(reason),
        )
        .unwrap();
        assert_eq!(visited, super::IpiReason::ALL);
        assert_eq!(super::visit_pending_reasons(1 << 7, |_| {}), Err(1 << 7));
    }

    #[cfg(feature = "ipi")]
    #[test]
    fn reason_handler_registration_has_one_stable_owner() {
        let handlers = super::IpiReasonHandlers::new();
        assert!(handlers.register(super::IpiReason::CpuMaintenance, first_reason_handler));
        assert!(handlers.register(super::IpiReason::CpuMaintenance, first_reason_handler));
        assert!(!handlers.register(super::IpiReason::CpuMaintenance, second_reason_handler));
        assert_eq!(
            handlers.handler(super::IpiReason::CpuMaintenance),
            first_reason_handler as usize
        );
    }

    #[cfg(feature = "ipi")]
    #[test]
    fn reason_target_validation_uses_admitted_topology_and_actual_identity() {
        let mut visited = [false; 4];
        super::visit_ipi_targets(
            &super::IpiTarget::AllExceptCurrent {
                cpu_id: 1,
                cpu_num: 4,
            },
            4,
            1,
            |cpu| visited[cpu] = true,
        )
        .unwrap();
        assert_eq!(visited, [true, false, true, true]);
        assert_eq!(
            super::visit_ipi_targets(&super::IpiTarget::Other { cpu_id: 4 }, 4, 1, |_| {}),
            Err(super::IpiReasonError::InvalidTarget {
                cpu_id: 4,
                cpu_num: 4,
            })
        );
        assert_eq!(
            super::visit_ipi_targets(
                &super::IpiTarget::AllExceptCurrent {
                    cpu_id: 0,
                    cpu_num: 5,
                },
                4,
                0,
                |_| {},
            ),
            Err(super::IpiReasonError::InvalidCpuCount {
                requested: 5,
                admitted: 4,
            })
        );
        assert_eq!(
            super::visit_ipi_targets(
                &super::IpiTarget::AllExceptCurrent {
                    cpu_id: 2,
                    cpu_num: 4,
                },
                4,
                1,
                |_| {},
            ),
            Err(super::IpiReasonError::IncorrectCurrentCpu {
                claimed: 2,
                actual: 1,
            })
        );
        assert_eq!(
            super::visit_ipi_targets(&super::IpiTarget::Other { cpu_id: 1 }, 4, 1, |_| {}),
            Err(super::IpiReasonError::CurrentCpuIsNotRemote { cpu_id: 1 })
        );
    }
}
