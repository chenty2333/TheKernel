//! Platform initialization stages.

use ax_plat::init::InitIf;

struct InitIfImpl;

#[impl_plat_interface]
impl InitIf for InitIfImpl {
    fn init_early(_cpu_id: usize, _arg: usize) {
        ax_cpu::init::init_trap();
        crate::console::init_early();
        crate::time::init_early();
    }

    #[cfg(feature = "smp")]
    fn init_early_secondary(_cpu_id: usize) {
        ax_cpu::init::init_trap();
        crate::time::init_early();
    }

    fn init_later(_cpu_id: usize, _arg: usize) {
        // Interrupt controller domains and the timer interrupt are set up by
        // `IrqIf::init_boot_irqs`, which axruntime calls before handlers are
        // registered.
    }

    #[cfg(feature = "smp")]
    fn init_later_secondary(_cpu_id: usize) {}
}
