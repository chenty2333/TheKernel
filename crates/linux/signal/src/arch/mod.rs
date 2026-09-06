#[cfg(not(target_arch = "x86_64"))]
compile_error!("thekernel-linux-signal supports only x86_64");

mod x86_64;
pub use self::x86_64::*;

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
        safe static thekernel_linux_signal_trampoline: [u8; 0];
    }

    thekernel_linux_signal_trampoline.as_ptr() as usize
}
