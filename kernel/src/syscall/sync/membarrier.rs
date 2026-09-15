use alloc::sync::Arc;
#[cfg(feature = "smp-tlb-shootdown")]
use core::sync::atomic::{AtomicBool, AtomicU64};
use core::sync::atomic::{Ordering, fence};

#[cfg(feature = "smp-tlb-shootdown")]
use axerrno::AxError;
use axerrno::{AxResult, LinuxError};
use axtask::current;
#[cfg(not(feature = "smp-tlb-shootdown"))]
use kernel_guard::NoPreemptIrqSave;

use crate::{
    mm::{MembarrierState, TlbState},
    task::AsThread,
};

/// Linux's membarrier command values. Keep these explicit instead of using
/// libc headers: the syscall ABI is part of the kernel surface and the
/// parser is also exercised by host-side Rust tests.
pub(crate) const MEMBARRIER_CMD_QUERY: i32 = 0;
pub(crate) const MEMBARRIER_CMD_GLOBAL: i32 = 1;
pub(crate) const MEMBARRIER_CMD_GLOBAL_EXPEDITED: i32 = 2;
pub(crate) const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: i32 = 4;
pub(crate) const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 = 8;
pub(crate) const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 = 16;
pub(crate) const MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE: i32 = 32;
pub(crate) const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE: i32 = 64;
pub(crate) const MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ: i32 = 128;
pub(crate) const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ: i32 = 256;
pub(crate) const MEMBARRIER_CMD_GET_REGISTRATIONS: i32 = 512;
pub(crate) const MEMBARRIER_CMD_FLAG_CPU: u32 = 1;

/// The command mask this kernel advertises from `MEMBARRIER_CMD_QUERY`.
///
/// Linux builds this mask as `MEMBARRIER_CMD_BITMASK` minus
/// `MEMBARRIER_PRIVATE_EXPEDITED_RSEQ_BITMASK` when `CONFIG_RSEQ` is unset, and
/// minus `MEMBARRIER_CMD_GLOBAL` on a `nohz_full` system. This kernel has no
/// `nohz_full` CPUs, so `MEMBARRIER_CMD_GLOBAL` stays advertised, while the RSEQ
/// pair is not: `membarrier_private_expedited()` refuses RSEQ with `-EINVAL`
/// unless `CONFIG_RSEQ` is enabled, and publishing an RSEQ observation into the
/// interrupted thread needs an IRQ-safe path this kernel does not have (see the
/// note on `Command::PrivateExpeditedRseq`). Advertising it would over-report.
pub(crate) const MEMBARRIER_SUPPORTED_COMMANDS: isize = (MEMBARRIER_CMD_GLOBAL
    | MEMBARRIER_CMD_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE
    | MEMBARRIER_CMD_GET_REGISTRATIONS) as isize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Command {
    Query,
    Global,
    GlobalExpedited,
    RegisterGlobalExpedited,
    PrivateExpedited,
    RegisterPrivateExpedited,
    PrivateExpeditedSyncCore,
    RegisterPrivateExpeditedSyncCore,
    /// `MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ` and its registration command.
    /// Linux returns `-EINVAL` for both when `CONFIG_RSEQ` is unset; this
    /// kernel's membarrier IPI handler is address-space scoped and cannot reach
    /// the interrupted thread's `ThreadRseq` (a `SpinNoIrq` the scheduler holds
    /// while publishing `PREEMPT`), so the pair reports the same `-EINVAL`
    /// instead of a guarantee it cannot keep.
    PrivateExpeditedRseq,
    RegisterPrivateExpeditedRseq,
    GetRegistrations,
}

/// Decodes one `membarrier` call in Linux's order.
///
/// `SYSCALL_DEFINE3(membarrier)` validates `flags` *before* it looks at `cmd`:
///
/// ```c
/// switch (cmd) {
/// case MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ:
///         if (unlikely(flags && flags != MEMBARRIER_CMD_FLAG_CPU))
///                 return -EINVAL;
///         break;
/// default:
///         if (unlikely(flags))
///                 return -EINVAL;
/// }
///
/// if (!(flags & MEMBARRIER_CMD_FLAG_CPU))
///         cpu_id = -1;
/// ```
///
/// In this kernel version only `MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ` takes
/// `MEMBARRIER_CMD_FLAG_CPU`; the other private expedited commands reject every
/// non-zero flag. `cpu_id` is therefore parsed but never consumed: a call that
/// passes the flag check reaches the RSEQ command, which this kernel refuses
/// with `-EINVAL` before any target CPU would be used.
fn parse_command(cmd: i32, flags: u32, _cpu_id: i32) -> AxResult<Command> {
    if cmd == MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ {
        if flags & !MEMBARRIER_CMD_FLAG_CPU != 0 {
            return Err(LinuxError::EINVAL.into());
        }
    } else if flags != 0 {
        return Err(LinuxError::EINVAL.into());
    }

    match cmd {
        MEMBARRIER_CMD_QUERY => Ok(Command::Query),
        MEMBARRIER_CMD_GLOBAL => Ok(Command::Global),
        MEMBARRIER_CMD_GLOBAL_EXPEDITED => Ok(Command::GlobalExpedited),
        MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED => Ok(Command::RegisterGlobalExpedited),
        MEMBARRIER_CMD_PRIVATE_EXPEDITED => Ok(Command::PrivateExpedited),
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED => Ok(Command::RegisterPrivateExpedited),
        MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE => Ok(Command::PrivateExpeditedSyncCore),
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE => {
            Ok(Command::RegisterPrivateExpeditedSyncCore)
        }
        MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ => Ok(Command::PrivateExpeditedRseq),
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ => Ok(Command::RegisterPrivateExpeditedRseq),
        MEMBARRIER_CMD_GET_REGISTRATIONS => Ok(Command::GetRegistrations),
        // Includes command combinations and unknown values.
        _ => Err(LinuxError::EINVAL.into()),
    }
}

pub fn sys_membarrier(cmd: i32, flags: u32, cpu_id: i32) -> AxResult<isize> {
    let command = parse_command(cmd, flags, cpu_id)?;
    let state = current().as_thread().proc_data.aspace_tlb_state();
    let membarrier = state.membarrier_state();
    match command {
        Command::Query => Ok(MEMBARRIER_SUPPORTED_COMMANDS),
        // `MEMBARRIER_CMD_GLOBAL` calls `synchronize_rcu()` and
        // `MEMBARRIER_CMD_GLOBAL_EXPEDITED` interrupts every CPU whose current
        // task belongs to a registered address space. Neither inspects the
        // caller's registration state nor a capability, and both return 0 on a
        // single-CPU system. This kernel has no `synchronize_rcu()`, so the
        // global IPI rendezvous provides the equivalent ordering guarantee for
        // both commands (see `issue_global`).
        Command::Global | Command::GlobalExpedited => issue_global(),
        Command::RegisterGlobalExpedited => {
            membarrier.register_global();
            Ok(0)
        }
        Command::RegisterPrivateExpedited => {
            membarrier.register_private();
            Ok(0)
        }
        Command::RegisterPrivateExpeditedSyncCore => {
            // Linux keeps the sync-core and ordinary private expedited
            // registrations independent.  A sync-core registration alone
            // must not authorize PRIVATE_EXPEDITED.
            membarrier.register_sync_core();
            Ok(0)
        }
        Command::GetRegistrations => Ok(membarrier.registrations() as isize),
        // `membarrier_private_expedited()` checks registration before it looks
        // at `cpu_id`, so an unregistered caller gets EPERM even for an
        // out-of-range CPU selector.
        Command::PrivateExpedited => {
            if !membarrier.private_registered() {
                return Err(LinuxError::EPERM.into());
            }
            issue_private(&state, false)
        }
        Command::PrivateExpeditedSyncCore => {
            if !membarrier.sync_core_registered() {
                return Err(LinuxError::EPERM.into());
            }
            issue_private(&state, true)
        }
        // `CONFIG_RSEQ` unset in Linux terms: the registration command reports
        // EINVAL before the state is touched, and the expedited command reports
        // the same EINVAL rather than a barrier without the RSEQ observation.
        Command::PrivateExpeditedRseq | Command::RegisterPrivateExpeditedRseq => {
            Err(LinuxError::EINVAL.into())
        }
    }
}

#[inline(always)]
fn full_memory_barrier() {
    // On x86 this lowers to a hardware full barrier (not compiler_fence),
    // which is the ordering edge required before and after the rendezvous.
    fence(Ordering::SeqCst);
}

#[inline(always)]
fn synchronize_core() {
    full_memory_barrier();
    // CPUID is serializing on x86 and is the architecture primitive used by
    // Linux's sync-core membarrier path.
    let _ = core::arch::x86_64::__cpuid(0);
    full_memory_barrier();
}

#[cfg(feature = "smp-tlb-shootdown")]
mod remote {
    use axhal::irq::{IpiReason, IpiTarget};
    use axsync::spin::SpinNoIrq;
    use axtask::{can_block_current, yield_now};
    use kernel_guard::NoPreempt;

    use super::*;

    const BARRIER_TIMEOUT_NS: u64 = 5_000_000_000;
    const RETRY_INTERVAL_NS: u64 = 1_000_000;

    static REQUEST_LOCK: AtomicBool = AtomicBool::new(false);
    static REQUEST_STATE: SpinNoIrq<Option<Arc<TlbState>>> = SpinNoIrq::new(None);

    #[repr(align(64))]
    struct CpuRequest {
        generation: AtomicU64,
        active: AtomicBool,
        sync_core: AtomicBool,
        /// Selects the global rendezvous, whose acknowledgements live in
        /// `GLOBAL_ACK` instead of an address space's `MembarrierState`.
        global: AtomicBool,
    }

    impl CpuRequest {
        const fn new() -> Self {
            Self {
                generation: AtomicU64::new(0),
                active: AtomicBool::new(false),
                sync_core: AtomicBool::new(false),
                global: AtomicBool::new(false),
            }
        }
    }

    static REQUESTS: [CpuRequest; axconfig::plat::MAX_CPU_NUM] =
        [const { CpuRequest::new() }; axconfig::plat::MAX_CPU_NUM];

    /// Acknowledgement slot for the process-wide `MEMBARRIER_CMD_GLOBAL` and
    /// `MEMBARRIER_CMD_GLOBAL_EXPEDITED` rendezvous.
    ///
    /// Linux implements `MEMBARRIER_CMD_GLOBAL` with `synchronize_rcu()`: every
    /// CPU passes through a quiescent state (and therefore a full barrier)
    /// between the caller's pre-call and post-call accesses. This kernel has no
    /// blocking RCU grace-period primitive, so the same guarantee is provided
    /// by an explicit IPI rendezvous. `MEMBARRIER_CMD_GLOBAL_EXPEDITED` shares
    /// it: Linux interrupts only the CPUs whose current task belongs to an
    /// address space that registered, while this rendezvous always interrupts
    /// every other online CPU. That target set is a superset of Linux's, so the
    /// ordering guarantee is at least as strong, at the cost of extra IPIs.
    static GLOBAL_GENERATION: AtomicU64 = AtomicU64::new(0);
    static GLOBAL_ACK: [AtomicU64; axconfig::plat::MAX_CPU_NUM] =
        [const { AtomicU64::new(0) }; axconfig::plat::MAX_CPU_NUM];

    fn lock_request() -> RequestGuard {
        // Keep the lock acquisition non-sleeping and usable from contexts that
        // cannot yield.  A caller in ordinary task context gives the current
        // issuer a scheduling opportunity after each short bounded spin batch;
        // unlike the old fixed-spin path, lock contention never becomes a
        // syscall-visible EAGAIN.  The issuer still has the fail-stop timeout
        // in `wait_for_barrier`, so a stalled rendezvous eventually releases the
        // lock without making this path depend on a blocking mutex.
        loop {
            if REQUEST_LOCK
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return RequestGuard;
            }

            for _ in 0..64 {
                if REQUEST_LOCK.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                } else {
                    break;
                }
            }

            // `yield_now` is a scheduling point, not a sleep, and lets the
            // owner make progress when both callers are runnable on one CPU.
            // IRQ/preemption-disabled callers must remain strictly nonblocking
            // and therefore simply continue with another bounded spin batch.
            if can_block_current() {
                yield_now();
            }
        }
    }

    struct RequestGuard;

    impl Drop for RequestGuard {
        fn drop(&mut self) {
            REQUEST_LOCK.store(false, Ordering::Release);
        }
    }

    fn clear_request(targets: &[bool; axconfig::plat::MAX_CPU_NUM]) {
        for (cpu, target) in targets.iter().enumerate() {
            if *target {
                REQUESTS[cpu].active.store(false, Ordering::Release);
            }
        }
        REQUEST_STATE.lock().take();
    }

    fn all_acknowledged(
        state: &MembarrierState,
        generation: u64,
        targets: &[bool; axconfig::plat::MAX_CPU_NUM],
    ) -> bool {
        targets
            .iter()
            .enumerate()
            .all(|(cpu, target)| !*target || state.acknowledged(cpu, generation))
    }

    fn all_globally_acknowledged(generation: u64, targets: &[bool; axconfig::plat::MAX_CPU_NUM]) -> bool {
        targets.iter().enumerate().all(|(cpu, target)| {
            !*target || GLOBAL_ACK[cpu].load(Ordering::Acquire) >= generation
        })
    }

    fn add_late_residents(
        state: &Arc<TlbState>,
        issuer_cpu: usize,
        generation: u64,
        sync_core: bool,
        targets: &mut [bool; axconfig::plat::MAX_CPU_NUM],
    ) -> AxResult<()> {
        let cpu_count = axhal::cpu_num().min(axconfig::plat::MAX_CPU_NUM);
        for cpu in 0..cpu_count {
            if cpu == issuer_cpu || targets[cpu] || !state.membarrier_resident_on(cpu) {
                continue;
            }
            targets[cpu] = true;
            REQUESTS[cpu]
                .generation
                .store(generation, Ordering::Relaxed);
            REQUESTS[cpu].sync_core.store(sync_core, Ordering::Relaxed);
            REQUESTS[cpu].active.store(true, Ordering::Release);
            axhal::irq::send_ipi_reason(IpiReason::Membarrier, IpiTarget::Other { cpu_id: cpu })
                .map_err(|_| AxError::from(LinuxError::EAGAIN))?;
        }
        Ok(())
    }

    fn publish_request(
        state: &Arc<TlbState>,
        generation: u64,
        sync_core: bool,
        targets: &[bool; axconfig::plat::MAX_CPU_NUM],
    ) -> AxResult<()> {
        *REQUEST_STATE.lock() = Some(state.clone());
        for (cpu, target) in targets.iter().enumerate() {
            if !*target {
                continue;
            }
            REQUESTS[cpu]
                .generation
                .store(generation, Ordering::Relaxed);
            REQUESTS[cpu].sync_core.store(sync_core, Ordering::Relaxed);
            REQUESTS[cpu].active.store(true, Ordering::Release);
        }

        for (cpu, target) in targets.iter().enumerate() {
            if !*target {
                continue;
            }
            axhal::irq::send_ipi_reason(IpiReason::Membarrier, IpiTarget::Other { cpu_id: cpu })
                .map_err(|_| AxError::from(LinuxError::EAGAIN))?;
        }
        Ok(())
    }

    /// Shared bounded wait for every flavour of the rendezvous.
    ///
    /// `progress` samples whether all targets acknowledged and may admit late
    /// arrivals before answering; `resend` re-issues the IPI to whatever is
    /// still missing. Both get the live target set so a single sampler can grow
    /// it.
    fn wait_for_barrier(
        targets: &mut [bool; axconfig::plat::MAX_CPU_NUM],
        mut progress: impl FnMut(&mut [bool; axconfig::plat::MAX_CPU_NUM]) -> AxResult<bool>,
        mut resend: impl FnMut(&[bool; axconfig::plat::MAX_CPU_NUM]) -> AxResult<()>,
    ) -> AxResult<()> {
        let started = axhal::time::monotonic_time_nanos();
        let deadline = started.saturating_add(BARRIER_TIMEOUT_NS);
        let mut next_retry = started.saturating_add(RETRY_INTERVAL_NS);
        loop {
            if progress(targets)? {
                // A second sample after a full fence closes the ordinary
                // scheduler-entry race without holding a scheduler lock across
                // the remote wait.
                fence(Ordering::SeqCst);
                if progress(targets)? {
                    return Ok(());
                }
            }
            let now = axhal::time::monotonic_time_nanos();
            if now >= deadline {
                return Err(LinuxError::EAGAIN.into());
            }
            if now >= next_retry {
                resend(targets)?;
                next_retry = now.saturating_add(RETRY_INTERVAL_NS);
            }
            core::hint::spin_loop();
        }
    }

    fn send_to(cpu: usize) -> AxResult<()> {
        axhal::irq::send_ipi_reason(IpiReason::Membarrier, IpiTarget::Other { cpu_id: cpu })
            .map_err(|_| AxError::from(LinuxError::EAGAIN))
    }

    /// Address-space rendezvous: every CPU this image is resident on, plus any
    /// CPU it becomes resident on while the barrier is in flight.
    fn wait_for_resident_ack(
        state_owner: &Arc<TlbState>,
        state: &MembarrierState,
        generation: u64,
        issuer_cpu: usize,
        sync_core_requested: bool,
        targets: &mut [bool; axconfig::plat::MAX_CPU_NUM],
    ) -> AxResult<()> {
        let mut progress = |targets: &mut [bool; axconfig::plat::MAX_CPU_NUM]| {
            add_late_residents(
                state_owner,
                issuer_cpu,
                generation,
                sync_core_requested,
                targets,
            )?;
            Ok(all_acknowledged(state, generation, targets))
        };
        let mut resend = |targets: &[bool; axconfig::plat::MAX_CPU_NUM]| {
            for (cpu, target) in targets.iter().enumerate() {
                if *target && !state.acknowledged(cpu, generation) {
                    send_to(cpu)?;
                }
            }
            Ok(())
        };
        wait_for_barrier(targets, &mut progress, &mut resend)
    }

    /// Process-wide rendezvous. No late arrival can join: the global command
    /// targets every other online CPU unconditionally.
    fn wait_for_global_ack(
        generation: u64,
        targets: &mut [bool; axconfig::plat::MAX_CPU_NUM],
    ) -> AxResult<()> {
        let mut progress = |targets: &mut [bool; axconfig::plat::MAX_CPU_NUM]| {
            Ok(all_globally_acknowledged(generation, targets))
        };
        let mut resend = |targets: &[bool; axconfig::plat::MAX_CPU_NUM]| {
            for (cpu, target) in targets.iter().enumerate() {
                if *target && GLOBAL_ACK[cpu].load(Ordering::Acquire) < generation {
                    send_to(cpu)?;
                }
            }
            Ok(())
        };
        wait_for_barrier(targets, &mut progress, &mut resend)
    }

    fn publish_global(
        generation: u64,
        sync_core: bool,
        targets: &[bool; axconfig::plat::MAX_CPU_NUM],
    ) -> AxResult<()> {
        // No address-space state: the handler acks through `GLOBAL_ACK`.
        *REQUEST_STATE.lock() = None;
        for (cpu, target) in targets.iter().enumerate() {
            if !*target {
                continue;
            }
            REQUESTS[cpu].generation.store(generation, Ordering::Relaxed);
            REQUESTS[cpu].sync_core.store(sync_core, Ordering::Relaxed);
            REQUESTS[cpu].global.store(true, Ordering::Relaxed);
            REQUESTS[cpu].active.store(true, Ordering::Release);
        }
        for (cpu, target) in targets.iter().enumerate() {
            if !*target {
                continue;
            }
            axhal::irq::send_ipi_reason(IpiReason::Membarrier, IpiTarget::Other { cpu_id: cpu })
                .map_err(|_| AxError::from(LinuxError::EAGAIN))?;
        }
        Ok(())
    }

    fn clear_global_request(targets: &[bool; axconfig::plat::MAX_CPU_NUM]) {
        for (cpu, target) in targets.iter().enumerate() {
            if *target {
                REQUESTS[cpu].global.store(false, Ordering::Relaxed);
                REQUESTS[cpu].active.store(false, Ordering::Release);
            }
        }
    }

    /// Issues the process-wide barrier behind `MEMBARRIER_CMD_GLOBAL` and
    /// `MEMBARRIER_CMD_GLOBAL_EXPEDITED`.
    pub(super) fn issue_global() -> AxResult<isize> {
        let cpu_count = axhal::cpu_num();
        let issuer_cpu = axhal::percpu::this_cpu_id();
        if cpu_count == 0 || cpu_count > axconfig::plat::MAX_CPU_NUM || issuer_cpu >= cpu_count {
            return Err(LinuxError::EOVERFLOW.into());
        }
        // Linux returns 0 without any barrier work on a single-CPU system.
        if cpu_count == 1 {
            return finish_local(false);
        }

        let _request_guard = lock_request();
        let mut targets = [false; axconfig::plat::MAX_CPU_NUM];
        let generation = {
            let _guard = NoPreempt::new();
            // Scenario (A)/(B) of the Linux comment block: the barrier must
            // precede the IPIs and follow them.
            full_memory_barrier();
            let generation = GLOBAL_GENERATION
                .try_update(Ordering::SeqCst, Ordering::Acquire, |current| {
                    current.checked_add(1)
                })
                .map_err(|_| AxError::from(LinuxError::EOVERFLOW))?
                + 1;
            GLOBAL_ACK[issuer_cpu].store(generation, Ordering::Release);
            for (cpu, target) in targets.iter_mut().enumerate().take(cpu_count) {
                if cpu != issuer_cpu {
                    *target = true;
                }
            }
            if let Err(error) = publish_global(generation, false, &targets) {
                clear_global_request(&targets);
                return Err(error);
            }
            generation
        };

        let result = wait_for_global_ack(generation, &mut targets);
        clear_global_request(&targets);
        result?;
        full_memory_barrier();
        Ok(0)
    }

    pub(super) fn init() {
        assert!(
            axhal::irq::register_ipi_reason(IpiReason::Membarrier, membarrier_ipi_handler),
            "failed to register the membarrier IPI reason"
        );
    }

    pub(super) fn issue(state: &Arc<TlbState>, sync_core_requested: bool) -> AxResult<isize> {
        let _request_guard = lock_request();
        let cpu_count = axhal::cpu_num();
        let issuer_cpu = axhal::percpu::this_cpu_id();
        if cpu_count == 0 || cpu_count > axconfig::plat::MAX_CPU_NUM || issuer_cpu >= cpu_count {
            return Err(LinuxError::EOVERFLOW.into());
        }

        let mut targets = [false; axconfig::plat::MAX_CPU_NUM];
        let generation = {
            // Keep the current task on the issuer CPU while the generation
            // and resident snapshot are published. The wait itself remains
            // preemptible, so a remote CPU can always run its handler.
            let _guard = NoPreempt::new();
            if sync_core_requested {
                synchronize_core();
            } else {
                full_memory_barrier();
            }
            let generation = state.membarrier_state().next_generation()?;
            state.membarrier_state().acknowledge(issuer_cpu, generation);
            for (cpu, target) in targets.iter_mut().enumerate().take(cpu_count) {
                if cpu != issuer_cpu && state.membarrier_resident_on(cpu) {
                    *target = true;
                }
            }
            if targets.iter().all(|target| !*target) {
                return finish_local(sync_core_requested);
            }
            if let Err(error) = publish_request(state, generation, sync_core_requested, &targets) {
                clear_request(&targets);
                return Err(error);
            }
            generation
        };

        let result = wait_for_resident_ack(
            state,
            state.membarrier_state(),
            generation,
            issuer_cpu,
            sync_core_requested,
            &mut targets,
        );
        clear_request(&targets);
        result?;
        if sync_core_requested {
            synchronize_core();
        } else {
            full_memory_barrier();
        }
        Ok(0)
    }

    fn finish_local(sync_core_requested: bool) -> AxResult<isize> {
        if sync_core_requested {
            synchronize_core();
        } else {
            full_memory_barrier();
        }
        Ok(0)
    }

    fn membarrier_ipi_handler() {
        let cpu = axhal::percpu::this_cpu_id();
        if cpu >= axconfig::plat::MAX_CPU_NUM || !REQUESTS[cpu].active.load(Ordering::Acquire) {
            return;
        }
        let generation = REQUESTS[cpu].generation.load(Ordering::Acquire);
        let sync_core_requested = REQUESTS[cpu].sync_core.load(Ordering::Relaxed);
        // `MEMBARRIER_CMD_GLOBAL`/`_GLOBAL_EXPEDITED` have no address space to
        // acknowledge against: `linux/ipi_mb()` only orders memory, and the
        // acknowledgement goes to the process-wide slot.
        if REQUESTS[cpu].global.load(Ordering::Relaxed) {
            if GLOBAL_ACK[cpu].load(Ordering::Acquire) >= generation {
                return;
            }
            full_memory_barrier();
            GLOBAL_ACK[cpu].store(generation, Ordering::Release);
            return;
        }
        let Some(state) = REQUEST_STATE.lock().clone() else {
            return;
        };
        let membarrier = state.membarrier_state();
        if membarrier.acknowledged(cpu, generation) {
            return;
        }
        if sync_core_requested {
            synchronize_core();
        } else {
            full_memory_barrier();
        }
        membarrier.acknowledge(cpu, generation);
    }
}

#[cfg(feature = "smp-tlb-shootdown")]
use remote::{init as init_remote, issue as issue_remote, issue_global as issue_global_remote};

#[cfg(feature = "smp-tlb-shootdown")]
pub(crate) fn init_membarrier_ipi() {
    init_remote();
}

#[cfg(not(feature = "smp-tlb-shootdown"))]
pub(crate) fn init_membarrier_ipi() {}

/// `MEMBARRIER_CMD_GLOBAL` and `MEMBARRIER_CMD_GLOBAL_EXPEDITED`.
///
/// Both are process-wide, so they never inspect the caller's registration
/// state or a capability; Linux returns 0 on a single-CPU system.
fn issue_global() -> AxResult<isize> {
    #[cfg(feature = "smp-tlb-shootdown")]
    {
        issue_global_remote()
    }

    #[cfg(not(feature = "smp-tlb-shootdown"))]
    {
        // Without the IPI machinery this build is single-CPU only, where Linux
        // also does no work at all.
        if axhal::cpu_num() > 1 {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        let _guard = NoPreemptIrqSave::new();
        full_memory_barrier();
        Ok(0)
    }
}

fn issue_private(state: &Arc<TlbState>, sync_core_requested: bool) -> AxResult<isize> {
    #[cfg(feature = "smp-tlb-shootdown")]
    {
        issue_remote(state, sync_core_requested)
    }

    #[cfg(not(feature = "smp-tlb-shootdown"))]
    {
        if axhal::cpu_num() > 1 {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        let generation = {
            let _guard = NoPreemptIrqSave::new();
            if sync_core_requested {
                synchronize_core();
            } else {
                full_memory_barrier();
            }
            let generation = state.membarrier_state().next_generation()?;
            state
                .membarrier_state()
                .acknowledge(axhal::percpu::this_cpu_id(), generation);
            generation
        };
        if sync_core_requested {
            synchronize_core();
        } else {
            full_memory_barrier();
        }
        let _ = generation;
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_reports_exactly_the_implemented_commands() {
        // Linux: `MEMBARRIER_CMD_BITMASK` minus the RSEQ pair when CONFIG_RSEQ
        // is unset, and minus MEMBARRIER_CMD_GLOBAL on a nohz_full system. This
        // kernel has no nohz_full CPUs and no IRQ-safe RSEQ publication path.
        assert_eq!(MEMBARRIER_SUPPORTED_COMMANDS, 639);
        assert_eq!(
            MEMBARRIER_SUPPORTED_COMMANDS
                & (MEMBARRIER_CMD_GLOBAL
                    | MEMBARRIER_CMD_GLOBAL_EXPEDITED
                    | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED) as isize,
            (MEMBARRIER_CMD_GLOBAL
                | MEMBARRIER_CMD_GLOBAL_EXPEDITED
                | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED) as isize
        );
        assert_eq!(
            MEMBARRIER_SUPPORTED_COMMANDS
                & (MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ
                    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ) as isize,
            0
        );
        // QUERY itself is never a bit in the answer.
        assert_eq!(MEMBARRIER_SUPPORTED_COMMANDS & MEMBARRIER_CMD_QUERY as isize, 0);
    }

    #[test]
    fn parser_accepts_the_cpu_flag_only_for_the_rseq_command() {
        // v7.2.3 `SYSCALL_DEFINE3(membarrier)`: only the RSEQ command accepts
        // MEMBARRIER_CMD_FLAG_CPU; every other command rejects any non-zero
        // flag. cpu_id plays no part in any answer this kernel can give.
        for cmd in [
            MEMBARRIER_CMD_QUERY,
            MEMBARRIER_CMD_GLOBAL,
            MEMBARRIER_CMD_GLOBAL_EXPEDITED,
            MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED,
            MEMBARRIER_CMD_PRIVATE_EXPEDITED,
            MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED,
            MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE,
            MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE,
            MEMBARRIER_CMD_GET_REGISTRATIONS,
        ] {
            assert!(
                parse_command(cmd, MEMBARRIER_CMD_FLAG_CPU, 0).is_err(),
                "command {cmd} must reject FLAG_CPU"
            );
        }
        assert_eq!(
            parse_command(MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ, MEMBARRIER_CMD_FLAG_CPU, 0),
            Ok(Command::PrivateExpeditedRseq)
        );
        assert_eq!(
            parse_command(MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ, 0, -1),
            Ok(Command::PrivateExpeditedRseq)
        );
    }

    #[test]
    fn parser_rejects_flags_and_unknown_commands() {
        // Any other bit in `flags` is EINVAL even for the RSEQ command, and
        // `flags` is validated before the command table is consulted.
        assert!(parse_command(MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ, 2, 0).is_err());
        assert!(parse_command(MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ, 3, 0).is_err());
        assert!(parse_command(1 << 20, MEMBARRIER_CMD_FLAG_CPU, -1).is_err());
        assert!(parse_command(1 << 20, 0, -1).is_err());
        assert!(
            parse_command(
                MEMBARRIER_CMD_PRIVATE_EXPEDITED | MEMBARRIER_CMD_GET_REGISTRATIONS,
                0,
                -1
            )
            .is_err()
        );
        // The decoded command set is exactly what QUERY advertises.
        for cmd in [
            MEMBARRIER_CMD_QUERY,
            MEMBARRIER_CMD_GLOBAL,
            MEMBARRIER_CMD_GLOBAL_EXPEDITED,
            MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED,
            MEMBARRIER_CMD_PRIVATE_EXPEDITED,
            MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED,
            MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE,
            MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE,
            MEMBARRIER_CMD_GET_REGISTRATIONS,
            MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ,
            MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ,
        ] {
            assert!(parse_command(cmd, 0, -1).is_ok(), "command {cmd} must decode");
        }
        assert_eq!(MEMBARRIER_CMD_QUERY, 0);
        assert_eq!(MEMBARRIER_SUPPORTED_COMMANDS, 639);
    }

    #[test]
    fn registration_state_keeps_every_mode_independent() {
        let state = MembarrierState::new();
        assert_eq!(state.registrations(), 0);
        state.register_sync_core();
        assert!(!state.private_registered());
        assert!(state.sync_core_registered());
        assert_eq!(
            state.registrations(),
            MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE as u32
        );
        assert_eq!(state.next_generation(), Ok(1));
        assert_eq!(state.next_generation(), Ok(2));

        let child = state.fork_clone();
        assert_eq!(child.registrations(), state.registrations());
        assert!(!child.private_registered());
        assert!(child.sync_core_registered());
        // A child inherits registration policy, but not the parent's
        // in-flight barrier generation or CPU acknowledgements.
        assert_eq!(child.next_generation(), Ok(1));

        state.register_private();
        assert!(state.private_registered());
        assert!(state.sync_core_registered());
        assert_eq!(
            state.registrations(),
            (MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
                | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE) as u32
        );
        state.register_global();
        assert_eq!(
            state.registrations(),
            (MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
                | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
                | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE) as u32
        );
        // Registering global expedited is idempotent, like Linux's
        // `if (membarrier_state & MEMBARRIER_STATE_GLOBAL_EXPEDITED_READY)
        // return 0;`.
        state.register_global();
        assert_eq!(
            state.registrations(),
            (MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
                | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
                | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE) as u32
        );
    }
}
