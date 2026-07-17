#[cfg(feature = "smp-tlb-shootdown")]
mod imp {
    use core::sync::atomic::{AtomicU64, Ordering};

    use axhal::irq::{IPI_IRQ, IpiTarget};
    use axtlb::{
        CPU_MAINTENANCE_REASON, CpuMaintenance, ShootdownGrace, ShootdownRequest, TlbShootdown,
    };
    use kernel_guard::NoPreempt;

    const SHOOTDOWN_TIMEOUT_NS: u64 = 5_000_000_000;
    const RETRY_INITIAL_NS: u64 = 1_000_000;
    const RETRY_MAX_NS: u64 = 128_000_000;
    const MAX_RETRY_ROUNDS: usize = 40;
    const SELF_SERVICE_SPINS: usize = 64;

    static SHOOTDOWN: TlbShootdown<{ axconfig::plat::MAX_CPU_NUM }> = TlbShootdown::new();
    static CPU_RUNTIME: [CpuRuntime; axconfig::plat::MAX_CPU_NUM] =
        [const { CpuRuntime::new() }; axconfig::plat::MAX_CPU_NUM];

    #[repr(align(64))]
    struct CpuRuntime {
        ipi_handler_entries: AtomicU64,
        next_retry_ns: AtomicU64,
        retry_interval_ns: AtomicU64,
    }

    impl CpuRuntime {
        const fn new() -> Self {
            Self {
                ipi_handler_entries: AtomicU64::new(0),
                next_retry_ns: AtomicU64::new(0),
                retry_interval_ns: AtomicU64::new(RETRY_INITIAL_NS),
            }
        }

        fn claim_retry(&self, now: u64) -> bool {
            let mut observed = self.next_retry_ns.load(Ordering::Acquire);
            loop {
                if now < observed {
                    return false;
                }
                let interval = self
                    .retry_interval_ns
                    .load(Ordering::Acquire)
                    .clamp(RETRY_INITIAL_NS, RETRY_MAX_NS);
                let replacement = now.saturating_add(interval);
                match self.next_retry_ns.compare_exchange_weak(
                    observed,
                    replacement,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        let _ = self.retry_interval_ns.fetch_update(
                            Ordering::AcqRel,
                            Ordering::Acquire,
                            |current| Some(current.saturating_mul(2).min(RETRY_MAX_NS)),
                        );
                        return true;
                    }
                    Err(actual) => observed = actual,
                }
            }
        }
    }

    struct ShootdownAttempts {
        initial: [bool; axconfig::plat::MAX_CPU_NUM],
        retries: [u8; axconfig::plat::MAX_CPU_NUM],
        ipi_entries_before: [u64; axconfig::plat::MAX_CPU_NUM],
        #[cfg(feature = "irq-continuation-diagnostics")]
        irq_diagnostics_before:
            [Option<axtask::IrqContinuationDiagnosticSnapshot>; axconfig::plat::MAX_CPU_NUM],
        rounds: usize,
    }

    impl ShootdownAttempts {
        const fn new() -> Self {
            Self {
                initial: [false; axconfig::plat::MAX_CPU_NUM],
                retries: [0; axconfig::plat::MAX_CPU_NUM],
                ipi_entries_before: [0; axconfig::plat::MAX_CPU_NUM],
                #[cfg(feature = "irq-continuation-diagnostics")]
                irq_diagnostics_before: [None; axconfig::plat::MAX_CPU_NUM],
                rounds: 0,
            }
        }
    }

    pub(crate) struct GlobalGrace {
        _grace: ShootdownGrace<'static, { axconfig::plat::MAX_CPU_NUM }>,
    }

    pub(crate) fn init() {
        let cpu_count = axhal::cpu_num();
        assert!(
            cpu_count > 0 && cpu_count <= axconfig::plat::MAX_CPU_NUM,
            "invalid TLB shootdown CPU topology: online={cpu_count}, capacity={}",
            axconfig::plat::MAX_CPU_NUM
        );
        assert!(
            axhal::irq::register(IPI_IRQ, ipi_handler),
            "failed to reserve the raw IPI IRQ for TLB shootdown"
        );
        for cpu in 0..cpu_count {
            SHOOTDOWN.publish_online(cpu).unwrap_or_else(|error| {
                panic!("failed to publish CPU {cpu} for TLB shootdown: {error:?}")
            });
        }

        // Validate the handler and every online mailbox before user mappings
        // can become shared across CPUs.
        drop(synchronize_cpu_maintenance(CpuMaintenance::TLB_AND_ICACHE));
    }

    fn maintain_local(maintenance: CpuMaintenance) {
        if maintenance.needs_tlb() {
            axhal::asm::flush_tlb(None);
        }
        if maintenance.needs_icache() {
            axhal::asm::flush_icache_all();
        }
    }

    fn synchronize_cpu_maintenance(maintenance: CpuMaintenance) -> GlobalGrace {
        // Page-table cursors may have flushed before reaching this adapter.
        // Repeat one full local maintenance operation while migration is pinned,
        // through issue. Waiting for remote grace remains preemptible; each
        // self-service sample below pins only its own short CPU-local section.
        let cpu_count = axhal::cpu_num();
        let (issuer_cpu, request, mut attempts, started) = {
            let _cpu_guard = NoPreempt::new();
            let issuer_cpu = axhal::percpu::this_cpu_id();
            let mut attempts = ShootdownAttempts::new();
            for cpu in 0..cpu_count {
                attempts.ipi_entries_before[cpu] =
                    CPU_RUNTIME[cpu].ipi_handler_entries.load(Ordering::Acquire);
                #[cfg(feature = "irq-continuation-diagnostics")]
                {
                    attempts.irq_diagnostics_before[cpu] =
                        axtask::irq_continuation_diagnostic_snapshot(cpu);
                }
            }
            maintain_local(maintenance);
            let request = SHOOTDOWN
                .issue_after_local_maintenance(issuer_cpu, maintenance)
                .unwrap_or_else(|error| {
                    panic!(
                        "failed to issue {maintenance:?} shootdown from CPU {issuer_cpu}: \
                         {error:?}"
                    )
                });
            let started = axhal::time::monotonic_time_nanos();
            for cpu in 0..cpu_count {
                if request.needs_kick(cpu) {
                    attempts.initial[cpu] = true;
                    axhal::irq::send_ipi(IPI_IRQ, IpiTarget::Other { cpu_id: cpu });
                }
            }
            (issuer_cpu, request, attempts, started)
        };

        let deadline = started.saturating_add(SHOOTDOWN_TIMEOUT_NS);
        let mut retry_interval = RETRY_INITIAL_NS;
        let mut next_retry = started.saturating_add(retry_interval);
        loop {
            // Two page-table writers may both enter with local IRQs disabled.
            // One short no-migration batch services the actual current CPU and
            // polls a fixed number of times. Dropping the guard between batches
            // preserves scheduler progress while preventing a bilateral wait.
            let grace = {
                let _cpu_guard = NoPreempt::new();
                service_cpu(axhal::percpu::this_cpu_id());
                let mut grace = request.try_complete();
                for _ in 1..SELF_SERVICE_SPINS {
                    if grace.is_some() {
                        break;
                    }
                    core::hint::spin_loop();
                    grace = request.try_complete();
                }
                grace
            };
            if let Some(grace) = grace {
                return GlobalGrace { _grace: grace };
            }
            let now = axhal::time::monotonic_time_nanos();
            if now >= deadline {
                if let Some(grace) = request.try_complete() {
                    return GlobalGrace { _grace: grace };
                }
                shootdown_timeout(&request, issuer_cpu, started, &attempts);
            }
            if now >= next_retry && attempts.rounds < MAX_RETRY_ROUNDS {
                let mut retry_deadline_reached = false;
                for cpu in 0..cpu_count {
                    let sent = {
                        let _cpu_guard = NoPreempt::new();
                        let retry_now = axhal::time::monotonic_time_nanos();
                        if retry_now >= deadline {
                            retry_deadline_reached = true;
                            false
                        } else if request.target_pending(cpu) && claim_retry(cpu, retry_now) {
                            axhal::irq::send_ipi(IPI_IRQ, IpiTarget::Other { cpu_id: cpu });
                            true
                        } else {
                            false
                        }
                    };
                    if retry_deadline_reached {
                        break;
                    }
                    if sent {
                        attempts.retries[cpu] = attempts.retries[cpu].saturating_add(1);
                    }
                }
                if retry_deadline_reached {
                    if let Some(grace) = request.try_complete() {
                        return GlobalGrace { _grace: grace };
                    }
                    shootdown_timeout(&request, issuer_cpu, started, &attempts);
                }
                attempts.rounds += 1;
                retry_interval = retry_interval.saturating_mul(2).min(RETRY_MAX_NS);
                next_retry = axhal::time::monotonic_time_nanos().saturating_add(retry_interval);
            }
            core::hint::spin_loop();
        }
    }

    pub(crate) fn synchronize_tlb() -> GlobalGrace {
        synchronize_cpu_maintenance(CpuMaintenance::TLB)
    }

    pub(crate) fn synchronize_icache() -> GlobalGrace {
        synchronize_cpu_maintenance(CpuMaintenance::ICACHE)
    }

    pub(crate) fn synchronize_tlb_and_icache() -> GlobalGrace {
        synchronize_cpu_maintenance(CpuMaintenance::TLB_AND_ICACHE)
    }

    pub(crate) fn retire_after_tlb_grace<T>(retired: T) {
        let grace = synchronize_tlb();
        drop(retired);
        drop(grace);
    }

    fn ipi_handler() {
        let cpu = axhal::percpu::this_cpu_id();
        CPU_RUNTIME[cpu]
            .ipi_handler_entries
            .fetch_add(1, Ordering::Relaxed);
        service_cpu(cpu);
    }

    fn claim_retry(cpu: usize, now: u64) -> bool {
        // The backoff belongs to the target, not one issuer. Concurrent and
        // newly arriving requests therefore cannot restart recovery at 1 ms.
        CPU_RUNTIME[cpu].claim_retry(now)
    }

    fn service_cpu(cpu: usize) {
        let reasons = SHOOTDOWN
            .take_pending_reasons(cpu)
            .unwrap_or_else(|_| axhal::power::system_off());
        if reasons == 0 {
            return;
        }
        if reasons & CPU_MAINTENANCE_REASON.bit() != 0 {
            SHOOTDOWN
                .service_maintenance(cpu, |maintenance| {
                    maintain_local(maintenance);
                })
                .unwrap_or_else(|_| axhal::power::system_off());
        }
        if reasons & !CPU_MAINTENANCE_REASON.bit() != 0 {
            axhal::power::system_off();
        }
    }

    fn shootdown_timeout(
        request: &ShootdownRequest<'_, { axconfig::plat::MAX_CPU_NUM }>,
        issuer_cpu: usize,
        started: u64,
        attempts: &ShootdownAttempts,
    ) -> ! {
        let now = axhal::time::monotonic_time_nanos();
        let epoch = request.epoch();
        error!(
            "TLB shootdown epoch {epoch} timed out: maintenance={:?} issuer_cpu={} current_cpu={} \
             elapsed_ns={} retry_rounds={}; refusing to reclaim mapping resources",
            request.maintenance(),
            issuer_cpu,
            axhal::percpu::this_cpu_id(),
            now.saturating_sub(started),
            attempts.rounds,
        );
        let mut first_incomplete = None;
        for cpu in 0..axhal::cpu_num() {
            if !request.targets(cpu) {
                continue;
            }
            match SHOOTDOWN.cpu_snapshot(cpu) {
                Ok(snapshot) => {
                    let pending = request.target_pending(cpu);
                    let ipi_entries = CPU_RUNTIME[cpu].ipi_handler_entries.load(Ordering::Acquire);
                    let ipi_entries_delta =
                        ipi_entries.saturating_sub(attempts.ipi_entries_before[cpu]);
                    error!(
                        "shootdown target CPU {cpu}: request_pending={} initial_attempt={} \
                         retry_attempts={} online={} draining={} admissions={} reasons={:#x} \
                         tlb_requested={} tlb_completed={} icache_requested={} \
                         icache_completed={} ipi_entries_before={} ipi_entries={} \
                         ipi_entries_delta={}",
                        pending,
                        attempts.initial[cpu],
                        attempts.retries[cpu],
                        snapshot.is_online(),
                        snapshot.is_draining(),
                        snapshot.admissions(),
                        snapshot.pending_reasons(),
                        snapshot.requested_tlb_epoch(),
                        snapshot.completed_tlb_epoch(),
                        snapshot.requested_icache_epoch(),
                        snapshot.completed_icache_epoch(),
                        attempts.ipi_entries_before[cpu],
                        ipi_entries,
                        ipi_entries_delta,
                    );
                    if first_incomplete.is_none() && pending {
                        first_incomplete = Some((cpu, snapshot));
                    }
                    #[cfg(feature = "irq-continuation-diagnostics")]
                    if pending {
                        log_irq_continuation_diagnostics(cpu, attempts.irq_diagnostics_before[cpu]);
                    }
                }
                Err(error) => error!("TLB CPU {cpu}: snapshot failed: {error:?}"),
            }
        }
        if let Some((cpu, snapshot)) = first_incomplete {
            let ipi_entries = CPU_RUNTIME[cpu].ipi_handler_entries.load(Ordering::Acquire);
            panic!(
                "TLB shootdown epoch {epoch} did not reach grace: maintenance={:?} target CPU \
                 {cpu} reasons={:#x} TLB completed={} request={} latest={} I-cache completed={} \
                 request={} latest={} initial_attempt={} retry_attempts={} ipi_entries_before={} \
                 ipi_entries={} ipi_entries_delta={}",
                request.maintenance(),
                snapshot.pending_reasons(),
                snapshot.completed_tlb_epoch(),
                epoch,
                snapshot.requested_tlb_epoch(),
                snapshot.completed_icache_epoch(),
                epoch,
                snapshot.requested_icache_epoch(),
                attempts.initial[cpu],
                attempts.retries[cpu],
                attempts.ipi_entries_before[cpu],
                ipi_entries,
                ipi_entries.saturating_sub(attempts.ipi_entries_before[cpu]),
            );
        }
        panic!(
            "TLB shootdown epoch {epoch} did not reach grace without an incomplete CPU snapshot"
        );
    }

    #[cfg(feature = "irq-continuation-diagnostics")]
    fn log_irq_continuation_diagnostics(
        cpu: usize,
        before: Option<axtask::IrqContinuationDiagnosticSnapshot>,
    ) {
        let Some(after) = axtask::irq_continuation_diagnostic_snapshot(cpu) else {
            error!("IRQ continuation diagnostics unavailable for CPU {cpu}");
            return;
        };
        let before = before.unwrap_or(axtask::IrqContinuationDiagnosticSnapshot {
            latest_sequence: 0,
            timer_events: 0,
            context_switches: 0,
            context_switch_returns: 0,
            irq_off_preempt_disables: 0,
            irq_off_preempt_enables: 0,
            irq_off_outermost_preempt_enables: 0,
            irq_off_preempt_enable_returns: 0,
            irq_off_preempt_checks: 0,
            irq_off_preempt_check_returns: 0,
            irq_off_yield_entries: 0,
            irq_off_yield_returns: 0,
            irq_off_idle_boundaries: 0,
        });
        error!(
            "IRQ continuation CPU {cpu}: latest_sequence={} timer_events={} timer_delta={} \
             switches={} switch_delta={} switch_returns={} switch_return_delta={} \
             irq_off_disables={} irq_off_disable_delta={} irq_off_enables={} \
             irq_off_enable_delta={} irq_off_outermost_enables={} \
             irq_off_outermost_enable_delta={} irq_off_enable_returns={} \
             irq_off_enable_return_delta={} irq_off_checks={} irq_off_check_delta={} \
             irq_off_check_returns={} irq_off_check_return_delta={} irq_off_yield_entries={} \
             irq_off_yield_entry_delta={} irq_off_yield_returns={} irq_off_yield_return_delta={} \
             irq_off_idle_boundaries={} irq_off_idle_boundary_delta={} \
             flags_bits=irq_enabled:0,idle:1,need_resched:2,resched_allowed:3,peer_idle:4",
            after.latest_sequence,
            after.timer_events,
            after.timer_events.saturating_sub(before.timer_events),
            after.context_switches,
            after
                .context_switches
                .saturating_sub(before.context_switches),
            after.context_switch_returns,
            after
                .context_switch_returns
                .saturating_sub(before.context_switch_returns),
            after.irq_off_preempt_disables,
            after
                .irq_off_preempt_disables
                .saturating_sub(before.irq_off_preempt_disables),
            after.irq_off_preempt_enables,
            after
                .irq_off_preempt_enables
                .saturating_sub(before.irq_off_preempt_enables),
            after.irq_off_outermost_preempt_enables,
            after
                .irq_off_outermost_preempt_enables
                .saturating_sub(before.irq_off_outermost_preempt_enables),
            after.irq_off_preempt_enable_returns,
            after
                .irq_off_preempt_enable_returns
                .saturating_sub(before.irq_off_preempt_enable_returns),
            after.irq_off_preempt_checks,
            after
                .irq_off_preempt_checks
                .saturating_sub(before.irq_off_preempt_checks),
            after.irq_off_preempt_check_returns,
            after
                .irq_off_preempt_check_returns
                .saturating_sub(before.irq_off_preempt_check_returns),
            after.irq_off_yield_entries,
            after
                .irq_off_yield_entries
                .saturating_sub(before.irq_off_yield_entries),
            after.irq_off_yield_returns,
            after
                .irq_off_yield_returns
                .saturating_sub(before.irq_off_yield_returns),
            after.irq_off_idle_boundaries,
            after
                .irq_off_idle_boundaries
                .saturating_sub(before.irq_off_idle_boundaries),
        );

        let first_sequence = after.latest_sequence.saturating_sub(15).max(1);
        for sequence in first_sequence..=after.latest_sequence {
            if let Some(event) = axtask::irq_continuation_diagnostic_event(cpu, sequence) {
                error!(
                    "IRQ continuation CPU {cpu} event: sequence={} kind={} task_id={} \
                     peer_task_id={} flags={:#x} preempt_disable_count={}",
                    event.sequence,
                    event.kind,
                    event.task_id,
                    event.peer_task_id,
                    event.flags,
                    event.preempt_disable_count,
                );
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use core::mem::align_of;

        use super::*;

        #[test]
        fn retry_gate_is_cacheline_isolated_and_exponentially_bounded() {
            assert!(align_of::<CpuRuntime>() >= 64);
            let runtime = CpuRuntime::new();
            let mut now = 1_000_000_000;
            let mut interval = RETRY_INITIAL_NS;

            for _ in 0..16 {
                assert!(runtime.claim_retry(now));
                assert!(!runtime.claim_retry(now));
                assert_eq!(
                    runtime.next_retry_ns.load(Ordering::Acquire),
                    now.saturating_add(interval)
                );
                interval = interval.saturating_mul(2).min(RETRY_MAX_NS);
                assert_eq!(runtime.retry_interval_ns.load(Ordering::Acquire), interval);
                now = runtime.next_retry_ns.load(Ordering::Acquire);
            }
            assert_eq!(interval, RETRY_MAX_NS);
        }
    }
}

#[cfg(not(feature = "smp-tlb-shootdown"))]
mod imp {
    pub(crate) struct GlobalGrace;

    pub(crate) fn init() {
        assert_eq!(
            axhal::cpu_num(),
            1,
            "multi-CPU kernels require the smp-tlb-shootdown feature"
        );
    }

    pub(crate) fn synchronize_tlb() -> GlobalGrace {
        axhal::asm::flush_tlb(None);
        GlobalGrace
    }

    pub(crate) fn synchronize_icache() -> GlobalGrace {
        axhal::asm::flush_icache_all();
        GlobalGrace
    }

    pub(crate) fn synchronize_tlb_and_icache() -> GlobalGrace {
        axhal::asm::flush_tlb(None);
        axhal::asm::flush_icache_all();
        GlobalGrace
    }

    pub(crate) fn retire_after_tlb_grace<T>(retired: T) {
        axhal::asm::flush_tlb(None);
        drop(retired);
    }
}

pub(crate) use imp::{
    init, retire_after_tlb_grace, synchronize_icache, synchronize_tlb, synchronize_tlb_and_icache,
};
