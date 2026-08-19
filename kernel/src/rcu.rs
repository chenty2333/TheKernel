//! Kernel-owned bounded RCU domains.
//!
//! Credential and task-local seccomp slots are separate bounded domains. This
//! module owns their boot registration and deferred-work adapters so the
//! reusable `axrcu` crate remains independent of Linux policy.

use alloc::sync::Arc;

use axconfig::plat::MAX_CPU_NUM;
use axerrno::{AxError, AxResult};
use axrcu::{EpochDomain, EpochPlatform, RcuError, RcuSlot};
use kernel_guard::{IrqSave, NoPreempt};
use thekernel_linux_seccomp::SeccompState;

pub(crate) const CREDENTIAL_RETIRE_CAPACITY: usize = 128;
pub(crate) const SECCOMP_RETIRE_CAPACITY: usize = 128;
pub(crate) struct KernelEpochPlatform;

unsafe impl EpochPlatform for KernelEpochPlatform {
    type PinGuard = NoPreempt;

    fn pin_current_cpu() -> Self::PinGuard {
        NoPreempt::new()
    }

    fn current_cpu() -> usize {
        axhal::percpu::this_cpu_id()
    }

    fn with_local_irqs_disabled<R>(operation: impl FnOnce() -> R) -> R {
        let guard = IrqSave::new();
        let result = operation();
        drop(guard);
        result
    }

    fn in_task_context() -> bool {
        !axhal::irq::in_irq_context()
    }

    fn in_preemptible_task_context() -> bool {
        // This includes the scheduler's task-running, IRQ, and preemption
        // depth checks. The generic domain additionally rejects an active
        // reader on this CPU before it can clear the pointer.
        axtask::can_block_current()
    }

    fn reader_quiescent() {
        crate::deferred_work::wake_policy_worker();
    }
}

type CredentialDomain = EpochDomain<KernelEpochPlatform, MAX_CPU_NUM, CREDENTIAL_RETIRE_CAPACITY>;
pub(crate) type CredentialRcuSlot<T> =
    RcuSlot<'static, T, KernelEpochPlatform, MAX_CPU_NUM, CREDENTIAL_RETIRE_CAPACITY>;
pub(crate) type CredentialRetireReservation =
    axrcu::RetireReservation<'static, KernelEpochPlatform, MAX_CPU_NUM, CREDENTIAL_RETIRE_CAPACITY>;
type SeccompDomain = EpochDomain<KernelEpochPlatform, MAX_CPU_NUM, SECCOMP_RETIRE_CAPACITY>;
pub(crate) type SeccompRcuSlot<T> =
    RcuSlot<'static, T, KernelEpochPlatform, MAX_CPU_NUM, SECCOMP_RETIRE_CAPACITY>;
pub(crate) type SeccompRetireReservation =
    axrcu::RetireReservation<'static, KernelEpochPlatform, MAX_CPU_NUM, SECCOMP_RETIRE_CAPACITY>;

static CREDENTIAL_DOMAIN: CredentialDomain = EpochDomain::new();
static SECCOMP_DOMAIN: SeccompDomain = EpochDomain::new();

fn map_error(error: RcuError) -> AxError {
    match error {
        RcuError::RetireCapacity => AxError::ResourceBusy,
        RcuError::UnregisteredCpu
        | RcuError::NotTaskContext
        | RcuError::CpuAlreadyRegistered
        | RcuError::CpuNotRegistered
        | RcuError::ReaderNestingOverflow
        | RcuError::EpochExhausted
        | RcuError::EmptySlot => AxError::BadState,
        RcuError::CpuBusy => AxError::ResourceBusy,
    }
}

/// Registers every online CPU before the first policy slot is published.
pub(crate) fn init() -> AxResult<()> {
    let online = axhal::cpu_num().max(1);
    if online > MAX_CPU_NUM {
        return Err(map_error(RcuError::UnregisteredCpu));
    }
    for cpu in 0..online {
        match CREDENTIAL_DOMAIN.register_cpu(cpu) {
            Ok(()) | Err(RcuError::CpuAlreadyRegistered) => {}
            Err(error) => return Err(map_error(error)),
        }
        match SECCOMP_DOMAIN.register_cpu(cpu) {
            Ok(()) | Err(RcuError::CpuAlreadyRegistered) => {}
            Err(error) => return Err(map_error(error)),
        }
    }
    Ok(())
}

/// Host tests construct credential fixtures without running the architecture
/// entry path. Registering the current CPU here is a cold-path convenience;
/// production boot still performs complete registration in [`init`].
pub(crate) fn ensure_current_cpu_registered() -> AxResult<()> {
    match CREDENTIAL_DOMAIN.register_cpu(axhal::percpu::this_cpu_id()) {
        Ok(()) | Err(RcuError::CpuAlreadyRegistered) => Ok(()),
        Err(error) => Err(map_error(error)),
    }
}

pub(crate) fn ensure_seccomp_current_cpu_registered() -> AxResult<()> {
    match SECCOMP_DOMAIN.register_cpu(axhal::percpu::this_cpu_id()) {
        Ok(()) | Err(RcuError::CpuAlreadyRegistered) => Ok(()),
        Err(error) => Err(map_error(error)),
    }
}

pub(crate) fn credential_slot<T>(initial: Arc<T>) -> CredentialRcuSlot<T> {
    CredentialRcuSlot::new(&CREDENTIAL_DOMAIN, initial)
}

pub(crate) fn seccomp_slot(
    initial: Option<Arc<SeccompState>>,
) -> AxResult<SeccompRcuSlot<SeccompState>> {
    ensure_seccomp_current_cpu_registered()?;
    match initial {
        Some(initial) => Ok(SeccompRcuSlot::new(&SECCOMP_DOMAIN, initial)),
        None => Ok(SeccompRcuSlot::empty(&SECCOMP_DOMAIN)),
    }
}

/// Returns true only when the FIFO front can be reclaimed immediately. A
/// grace-blocked queue is deliberately not reported as ready: its outermost
/// reader-exit callback wakes the policy worker when progress becomes possible.
pub(crate) fn credential_retire_pending() -> bool {
    CREDENTIAL_DOMAIN.has_reclaimable_pending()
}

pub(crate) fn seccomp_retire_pending() -> bool {
    SECCOMP_DOMAIN.has_reclaimable_pending()
}

/// Drains a bounded number of credential retire entries in task context.
pub(crate) fn drain_credential_retire(limit: usize) -> usize {
    CREDENTIAL_DOMAIN
        .drain(limit)
        .map(|status| status.dropped)
        .unwrap_or(0)
}

pub(crate) fn drain_seccomp_retire(limit: usize) -> usize {
    SECCOMP_DOMAIN
        .drain(limit)
        .map(|status| status.dropped)
        .unwrap_or(0)
}

/// Publication calls this after queue commit. The generic policy worker owns
/// all destructor execution; this only makes the worker observable promptly.
pub(crate) fn wake_credential_retire_worker() {
    crate::deferred_work::wake_policy_worker();
}

pub(crate) fn wake_seccomp_retire_worker() {
    crate::deferred_work::wake_policy_worker();
}
