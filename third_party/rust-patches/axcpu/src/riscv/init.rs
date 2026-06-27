//! Helper functions to initialize the CPU states on systems bootstrapping.

/// Initializes trap handling on the current CPU.
///
/// In detail, it initializes the trap vector on RISC-V platforms.
pub fn init_trap() {
    #[cfg(feature = "uspace")]
    crate::uspace_common::init_exception_table();
    unsafe extern "C" {
        fn trap_vector_base();
    }
    unsafe {
        #[cfg(feature = "uspace")]
        riscv::register::sstatus::set_sum();
        // The trap entry distinguishes S-mode traps from U-mode traps by
        // `sscratch == 0`. OpenSBI/QEMU do not guarantee this CSR is cleared
        // before entering the kernel, so initialize it before enabling the
        // trap vector; otherwise the first S-mode interrupt/fault may be
        // misclassified as a U-mode trap and save registers through a stale
        // scratch pointer.
        riscv::register::sscratch::write(0);
        crate::asm::write_trap_vector_base(trap_vector_base as *const () as usize);
    }
}
