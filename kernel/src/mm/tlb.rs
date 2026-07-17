#[cfg(feature = "smp-tlb-shootdown")]
mod imp {
    use axhal::irq::{IPI_IRQ, IpiTarget};
    use axtlb::{CPU_MAINTENANCE_REASON, CpuMaintenance, ShootdownGrace, TlbShootdown};

    const SHOOTDOWN_TIMEOUT_NS: u64 = 5_000_000_000;

    static SHOOTDOWN: TlbShootdown<{ axconfig::plat::MAX_CPU_NUM }> = TlbShootdown::new();

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
        axhal::asm::flush_tlb(None);
        axhal::asm::flush_icache_all();
        drop(synchronize_after_local_maintenance(
            CpuMaintenance::TLB_AND_ICACHE,
        ));
    }

    fn synchronize_after_local_maintenance(maintenance: CpuMaintenance) -> GlobalGrace {
        let issuer_cpu = axhal::percpu::this_cpu_id();
        let request = SHOOTDOWN
            .issue_after_local_maintenance(issuer_cpu, maintenance)
            .unwrap_or_else(|error| {
                panic!("failed to issue {maintenance:?} shootdown from CPU {issuer_cpu}: {error:?}")
            });

        let cpu_count = axhal::cpu_num();
        for cpu in 0..cpu_count {
            if request.needs_kick(cpu) {
                axhal::irq::send_ipi(IPI_IRQ, IpiTarget::Other { cpu_id: cpu });
            }
        }

        let deadline = axhal::time::monotonic_time_nanos().saturating_add(SHOOTDOWN_TIMEOUT_NS);
        loop {
            // Two page-table writers may both enter with local IRQs disabled.
            // Servicing this CPU's atomic mailbox here prevents a bilateral
            // wait without acquiring any address-space or allocator lock.
            service_cpu(issuer_cpu);
            if let Some(grace) = request.try_complete() {
                return GlobalGrace { _grace: grace };
            }
            if axhal::time::monotonic_time_nanos() >= deadline {
                shootdown_timeout(request.epoch());
            }
            core::hint::spin_loop();
        }
    }

    pub(crate) fn synchronize_after_local_flush() -> GlobalGrace {
        synchronize_after_local_maintenance(CpuMaintenance::TLB)
    }

    pub(crate) fn synchronize_after_local_icache() -> GlobalGrace {
        synchronize_after_local_maintenance(CpuMaintenance::ICACHE)
    }

    pub(crate) fn synchronize_after_local_tlb_and_icache() -> GlobalGrace {
        synchronize_after_local_maintenance(CpuMaintenance::TLB_AND_ICACHE)
    }

    pub(crate) fn retire_after_local_flush<T>(retired: T) {
        let grace = synchronize_after_local_flush();
        drop(retired);
        drop(grace);
    }

    fn ipi_handler() {
        service_cpu(axhal::percpu::this_cpu_id());
    }

    fn service_cpu(cpu: usize) {
        let reasons = SHOOTDOWN
            .take_pending_reasons(cpu)
            .unwrap_or_else(|_| axhal::power::system_off());
        if reasons & CPU_MAINTENANCE_REASON.bit() != 0 {
            SHOOTDOWN
                .service_maintenance(cpu, |maintenance| {
                    if maintenance.needs_tlb() {
                        axhal::asm::flush_tlb(None);
                    }
                    if maintenance.needs_icache() {
                        axhal::asm::flush_icache_all();
                    }
                })
                .unwrap_or_else(|_| axhal::power::system_off());
        }
        if reasons & !CPU_MAINTENANCE_REASON.bit() != 0 {
            axhal::power::system_off();
        }
    }

    fn shootdown_timeout(epoch: u64) -> ! {
        error!("TLB shootdown epoch {epoch} timed out; refusing to reclaim mapping resources");
        let mut first_incomplete = None;
        for cpu in 0..axhal::cpu_num() {
            match SHOOTDOWN.cpu_snapshot(cpu) {
                Ok(snapshot) => {
                    error!(
                        "shootdown CPU {cpu}: online={} draining={} admissions={} reasons={:#x} \
                         tlb_requested={} tlb_completed={} icache_requested={} icache_completed={}",
                        snapshot.is_online(),
                        snapshot.is_draining(),
                        snapshot.admissions(),
                        snapshot.pending_reasons(),
                        snapshot.requested_tlb_epoch(),
                        snapshot.completed_tlb_epoch(),
                        snapshot.requested_icache_epoch(),
                        snapshot.completed_icache_epoch(),
                    );
                    if first_incomplete.is_none()
                        && (snapshot.completed_tlb_epoch() < snapshot.requested_tlb_epoch()
                            || snapshot.completed_icache_epoch()
                                < snapshot.requested_icache_epoch())
                    {
                        first_incomplete = Some((cpu, snapshot));
                    }
                }
                Err(error) => error!("TLB CPU {cpu}: snapshot failed: {error:?}"),
            }
        }
        if let Some((cpu, snapshot)) = first_incomplete {
            panic!(
                "TLB shootdown epoch {epoch} did not reach grace: CPU {cpu} reasons={:#x} \
                 TLB={}/{} I-cache={}/{}",
                snapshot.pending_reasons(),
                snapshot.completed_tlb_epoch(),
                snapshot.requested_tlb_epoch(),
                snapshot.completed_icache_epoch(),
                snapshot.requested_icache_epoch(),
            );
        }
        panic!(
            "TLB shootdown epoch {epoch} did not reach grace without an incomplete CPU snapshot"
        );
    }
}

#[cfg(not(feature = "smp-tlb-shootdown"))]
mod imp {
    pub(crate) struct GlobalGrace;

    pub(crate) const fn init() {}

    pub(crate) const fn synchronize_after_local_flush() -> GlobalGrace {
        GlobalGrace
    }

    pub(crate) const fn synchronize_after_local_icache() -> GlobalGrace {
        GlobalGrace
    }

    pub(crate) const fn synchronize_after_local_tlb_and_icache() -> GlobalGrace {
        GlobalGrace
    }

    pub(crate) fn retire_after_local_flush<T>(retired: T) {
        drop(retired);
    }
}

pub(crate) use imp::{
    init, retire_after_local_flush, synchronize_after_local_flush, synchronize_after_local_icache,
    synchronize_after_local_tlb_and_icache,
};
