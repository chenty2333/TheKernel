use axplat::init::InitIf;

struct InitIfImpl;

#[impl_plat_interface]
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
        crate::time::init_primary();
        #[cfg(feature = "hwp")]
        let _ = crate::hwp::init_current();
    }

    /// Initializes the platform at the later stage for secondary cores.
    #[cfg(feature = "smp")]
    fn init_later_secondary(cpu_id: usize) {
        crate::apic::init_secondary(cpu_id);
        crate::time::init_secondary();
        #[cfg(feature = "hwp")]
        let _ = crate::hwp::init_current();
    }
}
