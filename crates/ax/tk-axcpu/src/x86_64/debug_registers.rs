//! IRQ-disabled snapshots for a temporary userspace debug-register overlay.

/// Physical DR0--DR3/DR7 image. This is not an untrusted ABI record; the kernel
/// validates addresses, access types and reserved bits before selecting it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct HardwareDebugRegisters {
    /// Linear comparator addresses.
    pub addresses: [u64; 4],
    /// Architectural control word, with invalid/reserved bits removed.
    pub control: u64,
}

impl HardwareDebugRegisters {
    /// Whether any comparator is enabled.
    pub fn enabled(&self) -> bool { self.control & 0xff != 0 }

    /// Snapshot the current CPU's kernel/perf image before an overlay.
    pub fn read() -> Self {
        #[cfg(target_os = "none")]
        {
            use x86_64::registers::debug::{DebugAddressRegister, Dr0, Dr1, Dr2, Dr3, Dr7};
            Self { addresses: [Dr0::read(), Dr1::read(), Dr2::read(), Dr3::read()], control: Dr7::read_raw() }
        }
        #[cfg(not(target_os = "none"))]
        Self::default()
    }

    /// Install atomically with respect to traps/preemption: disable first,
    /// replace addresses, then enable the fully prepared control word.
    pub fn install(&self) {
        #[cfg(target_os = "none")]
        {
            use x86_64::registers::debug::{DebugAddressRegister, Dr0, Dr1, Dr2, Dr3, Dr7};
            Dr7::write_raw(0);
            Dr0::write(self.addresses[0]); Dr1::write(self.addresses[1]);
            Dr2::write(self.addresses[2]); Dr3::write(self.addresses[3]);
            Dr7::write_raw(self.control);
        }
    }
}
