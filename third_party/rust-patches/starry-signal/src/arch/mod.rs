cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        mod x86_64;
        pub use self::x86_64::*;
    } else if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
        mod riscv;
        pub use self::riscv::*;
    } else if #[cfg(target_arch = "aarch64")]{
        mod aarch64;
        pub use self::aarch64::*;
    } else if #[cfg(target_arch = "loongarch64")] {
        mod loongarch64;
        pub use self::loongarch64::*;
    } else {
        compile_error!("Unsupported architecture");
    }
}

/// The reason a user-provided signal context cannot be restored safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalContextError {
    /// The saved instruction pointer is not a valid user instruction address.
    InvalidProgramCounter,
    /// The saved stack pointer is not a valid user stack address.
    InvalidStackPointer,
    /// The saved architecture status contains privileged or otherwise invalid state.
    InvalidProcessorState,
}

pub fn signal_trampoline_address() -> usize {
    unsafe extern "C" {
        safe static signal_trampoline: [u8; 0];
    }

    signal_trampoline.as_ptr() as usize
}
