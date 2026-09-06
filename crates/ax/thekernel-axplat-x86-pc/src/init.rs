use axplat::init::InitIf;

struct InitIfImpl;

#[cfg_attr(target_os = "none", impl_plat_interface)]
impl InitIf for InitIfImpl {
    /// Initializes the platform at the early stage for the primary core.
    ///
    /// This function should be called immediately after the kernel has booted,
    /// and performed earliest platform configuration and initialization (e.g.,
    /// early console, clocking).
    fn init_early(_cpu_id: usize, _mbi: usize) {
        axcpu::init::init_trap();
        crate::console::init();
        crate::time::init_early();
        // The platform entry runs before axruntime clears `.bss`; finalize the
        // initialized-data handoff after the early diagnostics are available,
        // then copy/use the memory map.  CPU topology discovery is intentionally
        // performed before the runtime replaces the temporary boot page table:
        // MADT itself can reside in Multiboot's ACPI-reclaimable memory and is
        // therefore not part of the usable-memory direct map.
        crate::boot_info::finish_handoff();
        crate::mem::init();
        crate::cpu::init_topology();
    }

    /// Initializes the platform at the early stage for secondary cores.
    #[cfg(feature = "smp")]
    fn init_early_secondary(_cpu_id: usize) {
        axcpu::init::init_trap();
    }

    /// Initializes the platform at the later stage for the primary core.
    ///
    /// This function should be called after the kernel has done part of its
    /// initialization (e.g, logging, memory management), and finalized the rest of
    /// platform configuration and initialization.
    fn init_later(cpu_id: usize, _arg: usize) {
        crate::apic::init_primary(cpu_id);
        assert!(
            crate::cpu::record_current_cpu_topology(cpu_id),
            "BSP topology record does not match logical CPU {cpu_id}"
        );
        crate::time::init_primary();
        init_cet_fleet_member();
        #[cfg(feature = "hwp")]
        init_hwp_fleet_member();
        #[cfg(feature = "pmu")]
        init_pmu_fleet_member();
        report_cpu_state(cpu_id);
    }

    /// Initializes the platform at the later stage for secondary cores.
    #[cfg(feature = "smp")]
    fn init_later_secondary(cpu_id: usize) {
        crate::apic::init_secondary(cpu_id);
        assert!(
            crate::cpu::record_current_cpu_topology(cpu_id),
            "AP topology record does not match logical CPU {cpu_id}"
        );
        crate::time::init_secondary();
        init_cet_fleet_member();
        #[cfg(feature = "hwp")]
        init_hwp_fleet_member();
        #[cfg(feature = "pmu")]
        init_pmu_fleet_member();
        report_cpu_state(cpu_id);
    }
}

/// CET follows the same read-only prepare / all-CPU commit contract as other
/// x86 fleet features.  Failure is a supported runtime downgrade, never a
/// boot failure.
fn init_cet_fleet_member() {
    if crate::cet::prepare_current().is_err() {
        crate::cet::abort_current();
        return;
    }
    let _ = crate::cet::commit_current();
}

/// Join the HWP startup fleet without waiting for CPUs that the BSP has not
/// started yet.  A failed prepare aborts the fleet; the prepare phase itself is
/// read-only, so no CPU can be left with a partially applied clamp.
#[cfg(feature = "hwp")]
fn init_hwp_fleet_member() {
    if crate::hwp::prepare_current().is_err() {
        crate::hwp::abort_current();
        return;
    }
    let _ = crate::hwp::commit_current();
}

/// PMU preparation is read-only.  A CPU that cannot prove the product PMU
/// contract aborts the entire fleet before any peer receives PMU ownership.
#[cfg(feature = "pmu")]
fn init_pmu_fleet_member() {
    if crate::pmu::prepare_current().is_err() {
        crate::pmu::abort_current();
        return;
    }
    // The CPU which commits the all-online-core PMU fleet becomes the package
    // owner that decodes the single Panther Lake discovery table.  A malformed
    // or unavailable table is a local uncore downgrade, never a boot failure.
    if crate::pmu::commit_current() {
        let _ = crate::perf_uncore::discover_current();
    }
}

/// Report observations from this CPU after its local initialization. Fleet
/// activation can still be pending; CR4.CET is not a user SHSTK commitment.
fn report_cpu_state(cpu_id: usize) {
    use core::arch::{
        asm,
        x86_64::{__cpuid, __cpuid_count},
    };

    // SAFETY: Called only from platform initialization in ring zero after
    // local APIC setup. Architectural MSRs are available on supported x86_64.
    let (one, seven, cr4, apic_base, efer) = unsafe {
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        (
            __cpuid(1),
            __cpuid_count(7, 0),
            cr4,
            x86::msr::rdmsr(0x1b),
            x86::msr::rdmsr(0xc000_0080),
        )
    };
    let osxsave = cr4 & (1 << 18) != 0;
    let xcr0 = if osxsave {
        let (lo, hi): (u32, u32);
        // SAFETY: CR4.OSXSAVE was read above; XCR0 is architectural index 0.
        unsafe {
            asm!("xgetbv", in("ecx") 0u32, out("eax") lo, out("edx") hi,
                 options(nomem, nostack, preserves_flags));
        }
        u64::from(lo) | (u64::from(hi) << 32)
    } else {
        0
    };
    let apic_enabled = apic_base & (1 << 11) != 0;
    let x2apic = apic_base & (1 << 10) != 0;
    let svr = if !apic_enabled {
        0
    } else if x2apic {
        // SAFETY: IA32_APIC_BASE confirms enabled x2APIC mode.
        unsafe { x86::msr::rdmsr(0x80f) as u32 }
    } else {
        let base = axplat::mem::phys_to_virt(axplat::mem::pa!((apic_base & 0xffff_f000) as usize));
        // SAFETY: The initialized xAPIC register window is mapped by platform
        // setup. Reading SVR does not change interrupt-controller state.
        unsafe { core::ptr::read_volatile((base.as_usize() + 0xf0) as *const u32) }
    };
    diagnostic_println!(
        "THEKERNEL_CPU_VISIBLE cpu={} hypervisor={} apic={} pcid={} invpcid={} xsave={} pku={} \
         cet_ss={}",
        cpu_id,
        one.ecx >> 31,
        (one.edx >> 9) & 1,
        (one.ecx >> 17) & 1,
        (seven.ebx >> 10) & 1,
        (one.ecx >> 26) & 1,
        (seven.ecx >> 3) & 1,
        (seven.ecx >> 7) & 1,
    );
    diagnostic_println!(
        "THEKERNEL_CPU_ENABLED cpu={} apic={} apic_software={} x2apic={} pcid={} osxsave={} \
         xcr0={:#x} pke={} cet_cr4={} syscall={}",
        cpu_id,
        u8::from(apic_enabled),
        (svr >> 8) & 1,
        u8::from(x2apic),
        (cr4 >> 17) & 1,
        u8::from(osxsave),
        xcr0,
        (cr4 >> 22) & 1,
        (cr4 >> 23) & 1,
        efer & 1,
    );
}
