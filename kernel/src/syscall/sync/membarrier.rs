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

/// The supported command mask intentionally excludes GLOBAL and RSEQ. The
/// former has no proven global rendezvous in this kernel; the latter needs
/// architecture/user-rseq integration beyond the private barrier here.
pub(crate) const MEMBARRIER_SUPPORTED_COMMANDS: isize = (MEMBARRIER_CMD_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE
    | MEMBARRIER_CMD_GET_REGISTRATIONS)
    as isize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Command {
    Query,
    PrivateExpedited,
    RegisterPrivateExpedited,
    PrivateExpeditedSyncCore,
    RegisterPrivateExpeditedSyncCore,
    GetRegistrations,
}

fn parse_command(cmd: i32, flags: u32, _cpu_id: i32) -> AxResult<Command> {
    // This implementation does not advertise RSEQ, so FLAG_CPU and every
    // other non-zero flag are invalid for all commands we accept. cpu_id is
    // deliberately ignored when flags are zero, matching Linux's API.
    if flags != 0 {
        return Err(LinuxError::EINVAL.into());
    }

    let command = match cmd {
        MEMBARRIER_CMD_QUERY => Command::Query,
        MEMBARRIER_CMD_PRIVATE_EXPEDITED => Command::PrivateExpedited,
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED => Command::RegisterPrivateExpedited,
        MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE => Command::PrivateExpeditedSyncCore,
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE => {
            Command::RegisterPrivateExpeditedSyncCore
        }
        MEMBARRIER_CMD_GET_REGISTRATIONS => Command::GetRegistrations,
        MEMBARRIER_CMD_GLOBAL
        | MEMBARRIER_CMD_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
        | MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ
        | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ => {
            return Err(LinuxError::EINVAL.into());
        }
        // Includes command combinations and unknown values.
        _ => return Err(LinuxError::EINVAL.into()),
    };
    Ok(command)
}

pub fn sys_membarrier(cmd: i32, flags: u32, cpu_id: i32) -> AxResult<isize> {
    let command = parse_command(cmd, flags, cpu_id)?;
    if command == Command::Query {
        return Ok(MEMBARRIER_SUPPORTED_COMMANDS);
    }

    let state = current().as_thread().proc_data.aspace_tlb_state();
    let membarrier = state.membarrier_state();
    match command {
        Command::Query => unreachable!(),
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
    }

    impl CpuRequest {
        const fn new() -> Self {
            Self {
                generation: AtomicU64::new(0),
                active: AtomicBool::new(false),
                sync_core: AtomicBool::new(false),
            }
        }
    }

    static REQUESTS: [CpuRequest; axconfig::plat::MAX_CPU_NUM] =
        [const { CpuRequest::new() }; axconfig::plat::MAX_CPU_NUM];

    fn lock_request() -> RequestGuard {
        // Keep the lock acquisition non-sleeping and usable from contexts that
        // cannot yield.  A caller in ordinary task context gives the current
        // issuer a scheduling opportunity after each short bounded spin batch;
        // unlike the old fixed-spin path, lock contention never becomes a
        // syscall-visible EAGAIN.  The issuer still has the fail-stop timeout
        // in `wait_for_ack`, so a stalled rendezvous eventually releases the
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

    fn wait_for_ack(
        state_owner: &Arc<TlbState>,
        state: &MembarrierState,
        generation: u64,
        issuer_cpu: usize,
        sync_core: bool,
        targets: &mut [bool; axconfig::plat::MAX_CPU_NUM],
    ) -> AxResult<()> {
        let started = axhal::time::monotonic_time_nanos();
        let deadline = started.saturating_add(BARRIER_TIMEOUT_NS);
        let mut next_retry = started.saturating_add(RETRY_INTERVAL_NS);
        loop {
            add_late_residents(state_owner, issuer_cpu, generation, sync_core, targets)?;
            if all_acknowledged(state, generation, targets) {
                // A second admission sample after a full fence closes the
                // ordinary scheduler-entry race without holding a scheduler
                // lock across the remote wait.
                fence(Ordering::SeqCst);
                add_late_residents(state_owner, issuer_cpu, generation, sync_core, targets)?;
            }
            if all_acknowledged(state, generation, targets) {
                return Ok(());
            }
            let now = axhal::time::monotonic_time_nanos();
            if now >= deadline {
                return Err(LinuxError::EAGAIN.into());
            }
            if now >= next_retry {
                for (cpu, target) in targets.iter().enumerate() {
                    if *target && !state.acknowledged(cpu, generation) {
                        axhal::irq::send_ipi_reason(
                            IpiReason::Membarrier,
                            IpiTarget::Other { cpu_id: cpu },
                        )
                        .map_err(|_| AxError::from(LinuxError::EAGAIN))?;
                    }
                }
                next_retry = now.saturating_add(RETRY_INTERVAL_NS);
            }
            core::hint::spin_loop();
        }
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

        let result = wait_for_ack(
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
use remote::{init as init_remote, issue as issue_remote};

#[cfg(feature = "smp-tlb-shootdown")]
pub(crate) fn init_membarrier_ipi() {
    init_remote();
}

#[cfg(not(feature = "smp-tlb-shootdown"))]
pub(crate) fn init_membarrier_ipi() {}

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
    fn query_advertises_only_proven_private_commands() {
        assert_eq!(MEMBARRIER_SUPPORTED_COMMANDS, 632);
        assert_eq!(
            MEMBARRIER_SUPPORTED_COMMANDS
                & (MEMBARRIER_CMD_GLOBAL
                    | MEMBARRIER_CMD_GLOBAL_EXPEDITED
                    | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
                    | MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ
                    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ) as isize,
            0
        );
    }

    #[test]
    fn parser_accepts_ignored_cpu_id_without_cpu_flag() {
        assert_eq!(
            parse_command(MEMBARRIER_CMD_QUERY, 0, i32::MIN),
            Ok(Command::Query)
        );
        assert_eq!(
            parse_command(MEMBARRIER_CMD_GET_REGISTRATIONS, 0, i32::MAX),
            Ok(Command::GetRegistrations)
        );
    }

    #[test]
    fn parser_rejects_flags_and_unadvertised_commands() {
        assert!(parse_command(MEMBARRIER_CMD_QUERY, MEMBARRIER_CMD_FLAG_CPU, -1).is_err());
        assert!(parse_command(MEMBARRIER_CMD_GLOBAL, 0, -1).is_err());
        assert!(parse_command(MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ, 0, -1).is_err());
        assert!(
            parse_command(
                MEMBARRIER_CMD_PRIVATE_EXPEDITED | MEMBARRIER_CMD_GET_REGISTRATIONS,
                0,
                -1
            )
            .is_err()
        );
    }

    #[test]
    fn registration_state_keeps_private_modes_independent() {
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
    }
}
