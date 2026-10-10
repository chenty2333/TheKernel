//! X86 platform adapter for the handle-based IRQ framework.
//!
//! The public registry owns boxed actions; this module supplies the platform
//! trampoline and ties its lifetime to the framework handle. Callers must
//! quiesce/mask a device-side source before freeing its final action.

use core::{
    arch::asm,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

pub use irq_framework::*;
use spin::{Mutex, Once};

/// Domain assigned to x86 CPU vectors by this adapter.
pub const X86_VECTOR_DOMAIN: IrqDomainId = IrqDomainId(0x86);

static REGISTRY: Once<Registry<X86IrqOps>> = Once::new();
static ADAPTER_LOCK: Mutex<()> = Mutex::new(());
static ACTION_COUNTS: [AtomicUsize; 256] = [const { AtomicUsize::new(0) }; 256];
static LINE_ENABLED: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];
static MSI_ROUTE: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

pub(super) fn owns(vector: usize) -> bool {
    ACTION_COUNTS
        .get(vector)
        .is_some_and(|count| count.load(Ordering::Acquire) != 0)
        || MSI_ROUTE
            .get(vector)
            .is_some_and(|route| route.load(Ordering::Acquire))
}

#[derive(Clone, Copy)]
struct X86IrqOps;

impl IrqOps for X86IrqOps {
    type LocalIrqState = usize;

    fn current_cpu(&self) -> CpuId {
        CpuId(crate::percpu::this_cpu_id())
    }

    fn cpu_online(&self, cpu: CpuId) -> bool {
        // The HAL does not currently publish a live-CPU mask. The configured
        // topology is the strongest available bound; operations needing a
        // remote target still fail closed in `run_on_cpu_sync`/`set_enabled`.
        cpu.0 < axconfig::plat::MAX_CPU_NUM
    }

    fn in_irq_context(&self) -> bool {
        super::in_irq_context()
    }

    fn local_irq_save(&self) -> Self::LocalIrqState {
        #[cfg(target_os = "none")]
        {
            let flags: usize;
            // SAFETY: the x86 interrupt gate and this matching restore preserve
            // the caller's IF state around the framework metadata spin lock.
            unsafe {
                asm!("pushfq", "pop {}", "cli", out(reg) flags);
            }
            flags
        }
        #[cfg(not(target_os = "none"))]
        {
            0
        }
    }

    fn local_irq_restore(&self, state: Self::LocalIrqState) {
        #[cfg(target_os = "none")]
        if state & (1 << 9) != 0 {
            // SAFETY: only restore IF when it was set on entry to the lock.
            unsafe { asm!("sti", options(nomem, nostack)) };
        }
        #[cfg(not(target_os = "none"))]
        let _ = state;
    }

    fn run_on_cpu_sync(
        &self,
        _cpu: CpuId,
        _f: unsafe fn(*mut ()),
        _arg: *mut (),
    ) -> Result<(), IrqError> {
        Err(IrqError::Unsupported)
    }

    fn set_affinity(&self, _irq: IrqId, _affinity: IrqAffinity) -> Result<(), IrqError> {
        Err(IrqError::Unsupported)
    }

    fn set_enabled(&self, irq: IrqId, cpu: Option<CpuId>, enabled: bool) -> Result<(), IrqError> {
        let vector = vector_for(irq)?;
        if cpu.is_some_and(|cpu| cpu != self.current_cpu()) {
            return Err(IrqError::Unsupported);
        }
        #[cfg(all(target_os = "none", feature = "defplat", not(feature = "myplat")))]
        {
            // MSI/MSI-X delivery is masked and enabled at the device table,
            // never through an IOAPIC redirection entry. Keep the framework's
            // logical line state without touching an unrelated controller.
            if !MSI_ROUTE[vector].load(Ordering::Acquire) {
                axplat::irq::set_enable(vector, enabled);
            }
            LINE_ENABLED[vector].store(enabled, Ordering::Release);
            Ok(())
        }
        #[cfg(not(all(target_os = "none", feature = "defplat", not(feature = "myplat"))))]
        {
            let _ = (vector, enabled);
            Err(IrqError::Unsupported)
        }
    }

    fn is_enabled(&self, irq: IrqId, cpu: Option<CpuId>) -> Result<bool, IrqError> {
        let vector = vector_for(irq)?;
        if cpu.is_some_and(|cpu| cpu != self.current_cpu()) {
            return Err(IrqError::Unsupported);
        }
        Ok(LINE_ENABLED[vector].load(Ordering::Acquire))
    }

    fn is_pending(&self, _irq: IrqId, _cpu: Option<CpuId>) -> Result<bool, IrqError> {
        Err(IrqError::Unsupported)
    }

    fn is_in_service(&self, _irq: IrqId, _cpu: Option<CpuId>) -> Result<bool, IrqError> {
        Err(IrqError::Unsupported)
    }

    fn relax(&self) {
        core::hint::spin_loop();
    }
}

fn registry() -> &'static Registry<X86IrqOps> {
    REGISTRY.call_once(|| Registry::new(X86IrqOps))
}

fn vector_for(irq: IrqId) -> Result<usize, IrqError> {
    if irq.domain != X86_VECTOR_DOMAIN {
        return Err(IrqError::InvalidIrq);
    }
    let vector = usize::try_from(irq.hwirq.0).map_err(|_| IrqError::InvalidIrq)?;
    if !(32..256).contains(&vector) {
        return Err(IrqError::InvalidIrq);
    }
    #[cfg(feature = "ipi")]
    if vector == axconfig::devices::IPI_IRQ {
        return Err(IrqError::InvalidIrq);
    }
    Ok(vector)
}

fn id_for_vector(vector: usize) -> Result<IrqId, IrqError> {
    let hwirq = u32::try_from(vector).map_err(|_| IrqError::InvalidIrq)?;
    let irq = IrqId::new(X86_VECTOR_DOMAIN, HwIrq(hwirq));
    vector_for(irq)?;
    Ok(irq)
}

fn device_irq_trampoline() {
    // `irq_handler` publishes the vector on the current CPU for the duration
    // of the platform's synchronous, no-argument callback ABI.
    // SAFETY: the IRQ trap handler sets this per-CPU slot immediately around
    // the synchronous platform callback, with interrupts masked on that CPU.
    let vector = unsafe { *super::CURRENT_IRQ_VECTOR.current_ref_raw() };
    let Ok(irq) = id_for_vector(vector) else {
        return;
    };
    // SAFETY: the matching trap-boundary hook set this slot before dispatch.
    let origin = if unsafe { *super::CURRENT_IRQ_ORIGIN.current_ref_raw() } != 0 {
        IrqOrigin::User
    } else {
        IrqOrigin::Kernel
    };
    let _: IrqOutcome = registry().dispatch(irq, CpuId(crate::percpu::this_cpu_id()), origin);
}

fn validate_registration(vector: usize) -> Result<IrqId, IrqError> {
    let irq = id_for_vector(vector)?;
    if super::IRQ_CONTEXT[vector].load(Ordering::Acquire) != 0 {
        return Err(IrqError::Busy);
    }
    Ok(irq)
}

fn register_action(
    vector: usize,
    request: IrqRequest,
    platform_bridge_reserved: bool,
) -> Result<IrqHandle, IrqError> {
    let result = register_action_inner(vector, request, platform_bridge_reserved);
    if result.is_err() && platform_bridge_reserved {
        cleanup_unpublished_msi(vector);
    }
    result
}

fn register_action_inner(
    vector: usize,
    request: IrqRequest,
    platform_bridge_reserved: bool,
) -> Result<IrqHandle, IrqError> {
    let irq = validate_registration(vector)?;
    if !super::ensure_irq_boundary_hook() {
        return Err(IrqError::Controller);
    }
    if super::in_irq_context() {
        return Err(IrqError::InIrqContext);
    }

    // The lock is not used in interrupt context; prevent task preemption so
    // another same-CPU task cannot spin on an owner that was switched out.
    let _execution_guard = kernel_guard::NoPreempt::new();
    let _adapter_guard = ADAPTER_LOCK.lock();
    let count = ACTION_COUNTS[vector].load(Ordering::Acquire);
    if count == 0 {
        if platform_bridge_reserved {
            LINE_ENABLED[vector].store(true, Ordering::Release);
        } else {
            #[cfg(all(target_os = "none", feature = "defplat", not(feature = "myplat")))]
            {
                if !axplat::irq::register(vector, device_irq_trampoline) {
                    return Err(IrqError::Busy);
                }
                // Platform registration enables an IOAPIC route. Recording
                // that initial state lets the upstream registry preserve it.
                LINE_ENABLED[vector].store(true, Ordering::Release);
            }
            #[cfg(not(all(target_os = "none", feature = "defplat", not(feature = "myplat"))))]
            return Err(IrqError::Unsupported);
        }
    } else if platform_bridge_reserved {
        return Err(IrqError::Busy);
    }

    match registry().request(irq, request) {
        Ok(handle) => {
            ACTION_COUNTS[vector].store(count + 1, Ordering::Release);
            Ok(handle)
        }
        Err(error) => {
            if count == 0 && !platform_bridge_reserved {
                LINE_ENABLED[vector].store(false, Ordering::Release);
                // The source is required to stay masked until the caller has
                // received a handle; no action has been published here.
                if super::unregister(vector).is_none() {
                    log::warn!("dynamic IRQ setup failed to retire vector {vector}");
                } else {
                    MSI_ROUTE[vector].store(false, Ordering::Release);
                }
            }
            Err(error)
        }
    }
}

fn cleanup_unpublished_msi(vector: usize) {
    let _execution_guard = kernel_guard::NoPreempt::new();
    let _adapter_guard = ADAPTER_LOCK.lock();
    if ACTION_COUNTS[vector].load(Ordering::Acquire) != 0 {
        return;
    }
    LINE_ENABLED[vector].store(false, Ordering::Release);
    if super::unregister_dynamic(vector).is_none() {
        // An ambiguous remap teardown intentionally leaves the vector,
        // handler, and MSI_ROUTE marker reserved rather than risking reuse.
        log::warn!("dynamic MSI setup failed to retire vector {vector}");
    } else {
        MSI_ROUTE[vector].store(false, Ordering::Release);
    }
}

/// Registers a dynamic action on an already provisioned x86 CPU vector.
///
/// This is for platform-routed device vectors. For PCI MSI/MSI-X, prefer
/// [`allocate_msi_dynamic`], which reserves the vector and installs this
/// trampoline before the device message can be published. Keep the device
/// source masked until this call returns.
pub fn request_device(vector: usize, request: IrqRequest) -> Result<IrqHandle, IrqError> {
    #[cfg(all(target_os = "none", feature = "defplat", not(feature = "myplat")))]
    {
        register_action(vector, request, false)
    }
    #[cfg(not(all(target_os = "none", feature = "defplat", not(feature = "myplat"))))]
    {
        let _ = (vector, request);
        Err(IrqError::Unsupported)
    }
}

/// Enables one registered IRQ action.
///
/// For MSI/MSI-X, this controls framework dispatch only; the device-side
/// message mask remains owned by the driver.
pub fn enable_device(handle: IrqHandle) -> Result<(), IrqError> {
    if super::in_irq_context() {
        return Err(IrqError::InIrqContext);
    }
    let vector = vector_for(handle.irq())?;
    let _execution_guard = kernel_guard::NoPreempt::new();
    let _adapter_guard = ADAPTER_LOCK.lock();
    if ACTION_COUNTS[vector].load(Ordering::Acquire) == 0 {
        return Err(IrqError::NotFound);
    }
    registry().enable(handle)
}

/// Disables one registered IRQ action.
///
/// For MSI/MSI-X, this controls framework dispatch only; the device-side
/// message mask remains owned by the driver.
pub fn disable_device(handle: IrqHandle) -> Result<(), IrqError> {
    if super::in_irq_context() {
        return Err(IrqError::InIrqContext);
    }
    let vector = vector_for(handle.irq())?;
    let _execution_guard = kernel_guard::NoPreempt::new();
    let _adapter_guard = ADAPTER_LOCK.lock();
    if ACTION_COUNTS[vector].load(Ordering::Acquire) == 0 {
        return Err(IrqError::NotFound);
    }
    registry().disable(handle)
}

/// Waits for framework callbacks and native hard-IRQ boundaries to finish.
pub fn synchronize_device(handle: IrqHandle) -> Result<(), IrqError> {
    if super::in_irq_context() {
        return Err(IrqError::InIrqContext);
    }
    let vector = vector_for(handle.irq())?;
    let _execution_guard = kernel_guard::NoPreempt::new();
    let _adapter_guard = ADAPTER_LOCK.lock();
    if ACTION_COUNTS[vector].load(Ordering::Acquire) == 0 {
        return Err(IrqError::NotFound);
    }
    registry().synchronize(handle)?;
    super::synchronize_hardirq(vector);
    Ok(())
}

/// Frees an action; freeing the final action also unregisters its platform
/// trampoline and releases any MSI interrupt-remapping entry owned by it.
///
/// Before freeing the final MSI action, the caller must mask the device-side
/// MSI/MSI-X source and quiesce DMA/producer activity that could emit another
/// message. The function disables framework dispatch and any IOAPIC route it
/// owns, drains in-flight framework callbacks and native hard-IRQ boundaries,
/// then retires the platform vector and remapping entry. MSI/MSI-X source
/// masking remains the caller's responsibility.
pub fn free_device(handle: IrqHandle) -> Result<(), IrqError> {
    if super::in_irq_context() {
        return Err(IrqError::InIrqContext);
    }
    let vector = vector_for(handle.irq())?;
    // Keep the adapter lock pinned until release completes; see
    // `register_action` for why this guard must be declared first.
    let _execution_guard = kernel_guard::NoPreempt::new();
    let _adapter_guard = ADAPTER_LOCK.lock();
    let count = ACTION_COUNTS[vector].load(Ordering::Acquire);
    if count == 0 {
        return Err(IrqError::NotFound);
    }

    if count > 1 {
        registry().free(handle)?;
        ACTION_COUNTS[vector].store(count - 1, Ordering::Release);
        return Ok(());
    }

    // Keep the registration handle live if remapping teardown fails: callers
    // can correct the device-side state and retry without losing ownership.
    registry().disable(handle)?;
    registry().synchronize(handle)?;
    super::synchronize_hardirq(vector);
    if super::unregister_dynamic(vector).is_none()
        && super::MSI_OWNED[vector].load(Ordering::Acquire)
    {
        return Err(IrqError::Controller);
    }
    // After disable+synchronize above, this handle cannot have been removed by
    // another adapter caller (all lifecycle operations serialize on
    // ADAPTER_LOCK). The vector is masked and the callback is drained, so the
    // framework free is infallible; keep MSI_ROUTE set through its line-state
    // update to avoid treating an MSI vector as an IOAPIC route.
    registry()
        .free(handle)
        .expect("disabled and synchronized dynamic IRQ handle remains live");
    MSI_ROUTE[vector].store(false, Ordering::Release);
    ACTION_COUNTS[vector].store(0, Ordering::Release);
    LINE_ENABLED[vector].store(false, Ordering::Release);
    Ok(())
}

/// Reserves a PCI MSI vector whose callback dispatches registered actions.
///
/// The message may be programmed into the device only after this returns. The
/// returned handle owns the framework action, platform vector, and interrupt
/// remapping route; pass it to [`free_device`] only after masking the source.
pub fn allocate_msi_dynamic(
    requester: tk_vtd::PciRequester,
    request: IrqRequest,
) -> Option<(u64, u32, IrqHandle)> {
    #[cfg(all(target_os = "none", feature = "defplat", not(feature = "myplat")))]
    {
        let (address, data, vector) = super::allocate_msi_source(
            super::MsiSource::Pci(requester),
            None,
            device_irq_trampoline,
        )?;
        MSI_ROUTE[vector].store(true, Ordering::Release);
        match register_action(vector, request, true) {
            Ok(handle) => Some((address, data, handle)),
            Err(_) => None,
        }
    }
    #[cfg(not(all(target_os = "none", feature = "defplat", not(feature = "myplat"))))]
    {
        let _ = (requester, request);
        None
    }
}

/// Returns the framework's status snapshot for an action.
pub fn status_device(handle: IrqHandle) -> Result<IrqStatus, IrqError> {
    if super::in_irq_context() {
        return Err(IrqError::InIrqContext);
    }
    let vector = vector_for(handle.irq())?;
    let _execution_guard = kernel_guard::NoPreempt::new();
    let _adapter_guard = ADAPTER_LOCK.lock();
    if ACTION_COUNTS[vector].load(Ordering::Acquire) == 0 {
        return Err(IrqError::NotFound);
    }
    registry().status(handle)
}

#[cfg(test)]
mod tests {
    use super::{
        HwIrq, IrqDomainId, IrqError, IrqId, X86_VECTOR_DOMAIN, id_for_vector, vector_for,
    };

    #[test]
    fn vector_adapter_rejects_exceptions_and_non_x86_domains() {
        let external = (32..256)
            .find(|vector| id_for_vector(*vector).is_ok())
            .expect("x86 has at least one device vector");
        assert_eq!(id_for_vector(external).unwrap().domain, X86_VECTOR_DOMAIN);
        assert_eq!(id_for_vector(31), Err(IrqError::InvalidIrq));
        assert_eq!(id_for_vector(256), Err(IrqError::InvalidIrq));
        assert_eq!(
            vector_for(IrqId::new(IrqDomainId(7), HwIrq(0x40))),
            Err(IrqError::InvalidIrq)
        );
    }

    #[cfg(feature = "ipi")]
    #[test]
    fn vector_adapter_does_not_claim_the_raw_ipi_lane() {
        assert_eq!(
            id_for_vector(axconfig::devices::IPI_IRQ),
            Err(IrqError::InvalidIrq)
        );
    }
}
