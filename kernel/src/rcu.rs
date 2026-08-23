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

// Host tests map every OS thread onto percpu CPU 0. An epoch domain requires
// one execution context per registered CPU, and the test harness runs tests
// and their helper threads concurrently, so colliding identities corrupt the
// reader admission state. Test domains therefore model a bounded set of host
// execution contexts rather than inheriting the product's CPU-count limit.
#[cfg(test)]
extern crate std;

#[cfg(test)]
const HOST_TEST_RCU_CPU_SLOTS: usize = 64;

#[cfg(test)]
static HOST_TEST_RCU_CPU_SLOTS_IN_USE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
struct HostTestCpuLease {
    cpu: usize,
}

#[cfg(test)]
impl HostTestCpuLease {
    fn claim() -> Self {
        use core::sync::atomic::Ordering;

        loop {
            let occupied = HOST_TEST_RCU_CPU_SLOTS_IN_USE.load(Ordering::Acquire);
            let available = !occupied;
            if available == 0 {
                panic!(
                    "host RCU test fixture exhausted {HOST_TEST_RCU_CPU_SLOTS} concurrent CPU \
                     slots"
                );
            }
            let cpu = available.trailing_zeros() as usize;
            let claimed = occupied | (1_u64 << cpu);
            if HOST_TEST_RCU_CPU_SLOTS_IN_USE
                .compare_exchange_weak(occupied, claimed, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Self { cpu };
            }
        }
    }
}

#[cfg(test)]
impl Drop for HostTestCpuLease {
    fn drop(&mut self) {
        use core::sync::atomic::Ordering;

        unregister_host_test_cpu(self.cpu);

        let mask = 1_u64 << self.cpu;
        let previous = HOST_TEST_RCU_CPU_SLOTS_IN_USE.fetch_and(!mask, Ordering::Release);
        debug_assert_ne!(previous & mask, 0, "host RCU test CPU slot released twice");
    }
}

#[cfg(test)]
std::thread_local! {
    // Thread-local destruction returns this virtual CPU to the bounded test
    // fixture. Reuse is safe only after the old host thread has exited, so no
    // two live host threads can alias a reader state.
    static HOST_TEST_CPU: HostTestCpuLease = HostTestCpuLease::claim();
}

#[cfg(test)]
const RCU_DOMAIN_CPU_SLOTS: usize = HOST_TEST_RCU_CPU_SLOTS;

#[cfg(not(test))]
const RCU_DOMAIN_CPU_SLOTS: usize = MAX_CPU_NUM;

fn effective_cpu() -> usize {
    #[cfg(test)]
    {
        HOST_TEST_CPU.with(|lease| lease.cpu)
    }
    #[cfg(not(test))]
    axhal::percpu::this_cpu_id()
}

unsafe impl EpochPlatform for KernelEpochPlatform {
    type PinGuard = NoPreempt;

    fn pin_current_cpu() -> Self::PinGuard {
        NoPreempt::new()
    }

    fn current_cpu() -> usize {
        let cpu = effective_cpu();
        #[cfg(test)]
        register_host_test_cpu(cpu);
        cpu
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
        #[cfg(test)]
        {
            // Host unit tests execute RCU cleanup on ordinary `std` threads,
            // outside the kernel scheduler's current-task registry. Those
            // threads are preemptible task contexts for the test platform;
            // the generic domain still rejects an active local reader before
            // reclamation can proceed.
            return true;
        }

        #[cfg(not(test))]
        // This includes the scheduler's task-running, IRQ, and preemption
        // depth checks. The generic domain additionally rejects an active
        // reader on this CPU before it can clear the pointer.
        {
            axtask::can_block_current()
        }
    }

    fn reader_quiescent() {
        crate::deferred_work::wake_policy_worker();
    }
}

type CredentialDomain =
    EpochDomain<KernelEpochPlatform, RCU_DOMAIN_CPU_SLOTS, CREDENTIAL_RETIRE_CAPACITY>;
pub(crate) type CredentialRcuSlot<T> =
    RcuSlot<'static, T, KernelEpochPlatform, RCU_DOMAIN_CPU_SLOTS, CREDENTIAL_RETIRE_CAPACITY>;
pub(crate) type CredentialRetireReservation = axrcu::RetireReservation<
    'static,
    KernelEpochPlatform,
    RCU_DOMAIN_CPU_SLOTS,
    CREDENTIAL_RETIRE_CAPACITY,
>;
type SeccompDomain =
    EpochDomain<KernelEpochPlatform, RCU_DOMAIN_CPU_SLOTS, SECCOMP_RETIRE_CAPACITY>;
pub(crate) type SeccompRcuSlot<T> =
    RcuSlot<'static, T, KernelEpochPlatform, RCU_DOMAIN_CPU_SLOTS, SECCOMP_RETIRE_CAPACITY>;
pub(crate) type SeccompRetireReservation = axrcu::RetireReservation<
    'static,
    KernelEpochPlatform,
    RCU_DOMAIN_CPU_SLOTS,
    SECCOMP_RETIRE_CAPACITY,
>;

static CREDENTIAL_DOMAIN: CredentialDomain = EpochDomain::new();
static SECCOMP_DOMAIN: SeccompDomain = EpochDomain::new();

#[cfg(test)]
fn register_host_test_cpu(cpu: usize) {
    for result in [
        CREDENTIAL_DOMAIN.register_cpu(cpu),
        SECCOMP_DOMAIN.register_cpu(cpu),
    ] {
        match result {
            Ok(()) | Err(RcuError::CpuAlreadyRegistered) => {}
            Err(error) => panic!("host RCU test CPU registration failed: {error:?}"),
        }
    }
}

#[cfg(test)]
fn unregister_host_test_cpu(cpu: usize) {
    for (domain, result) in [
        ("credential", CREDENTIAL_DOMAIN.unregister_cpu(cpu)),
        ("seccomp", SECCOMP_DOMAIN.unregister_cpu(cpu)),
    ] {
        match result {
            // A thread may have claimed a host fixture slot without ever
            // entering RCU, or may have initialized only one policy slot.
            Ok(()) | Err(RcuError::CpuNotRegistered) => {}
            // Do not return the fixture slot if the domain still observes an
            // active reader. Reuse would turn a lifecycle violation into a
            // cross-thread reader-state alias.
            Err(RcuError::CpuBusy) => {
                panic!("host RCU test {domain} CPU {cpu} still has an active reader")
            }
            Err(error) => panic!("host RCU test {domain} CPU {cpu} unregister failed: {error:?}"),
        }
    }
}

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
    match CREDENTIAL_DOMAIN.register_cpu(effective_cpu()) {
        Ok(()) | Err(RcuError::CpuAlreadyRegistered) => Ok(()),
        Err(error) => Err(map_error(error)),
    }
}

pub(crate) fn ensure_seccomp_current_cpu_registered() -> AxResult<()> {
    match SECCOMP_DOMAIN.register_cpu(effective_cpu()) {
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

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn host_threads_unregister_rcu_domains_before_slot_reuse() {
        ensure_current_cpu_registered().unwrap();
        let slot = Arc::new(credential_slot(Arc::new(0_usize)));

        // Reuse the same bounded host fixture slot through more rounds than
        // the retire FIFO can hold. Each joined thread must have unregistered
        // both domains before the next one can reuse its virtual CPU.
        for value in 1..=CREDENTIAL_RETIRE_CAPACITY + 1 {
            let reader_slot = slot.clone();
            let child_cpu = thread::spawn(move || {
                assert_eq!(*reader_slot.load(), value - 1);
                effective_cpu()
            })
            .join()
            .unwrap();

            assert_eq!(
                CREDENTIAL_DOMAIN.unregister_cpu(child_cpu),
                Err(RcuError::CpuNotRegistered)
            );
            assert_eq!(
                SECCOMP_DOMAIN.unregister_cpu(child_cpu),
                Err(RcuError::CpuNotRegistered)
            );

            let expected = slot.load();
            let reservation = slot.reserve_retire().unwrap();
            let retired = slot
                .publish(Arc::new(value), &expected, reservation)
                .unwrap();
            drop(expected);
            drop(retired);
            assert_eq!(drain_credential_retire(1), 1);
        }
    }
}
