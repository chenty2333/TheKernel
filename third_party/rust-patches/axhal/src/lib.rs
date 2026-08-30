//! [ArceOS] hardware abstraction layer, provides unified APIs for
//! platform-specific operations.
//!
//! It does the bootstrapping and initialization process for the specified
//! platform, and provides useful operations on the hardware.
//!
//! Currently supported platforms (specify by cargo features):
//!
//! - `x86-pc`: Standard PC with x86_64 ISA.
//! - `dummy`: If none of the above platform is selected, the dummy platform
//!   will be used. In this platform, most of the operations are no-op or
//!   `unimplemented!()`. This platform is mainly used for [cargo test].
//!
//! # Cargo Features
//!
//! - `smp`: Enable SMP (symmetric multiprocessing) support.
//! - `fp-simd`: Enable floating-point and SIMD support.
//! - `paging`: Enable page table manipulation.
//! - `irq`: Enable interrupt handling support.
//! - `ipi`: Enable the typed inter-processor interrupt reason broker.
//! - `tls`: Enable kernel space thread-local storage support.
//! - `rtc`: Enable real-time clock support.
//! - `uspace`: Enable user space support.
//!
//! [ArceOS]: https://github.com/arceos-org/arceos
//! [cargo test]: https://doc.rust-lang.org/cargo/guide/tests.html

#![no_std]
#![feature(doc_cfg)]

#[cfg(not(target_arch = "x86_64"))]
compile_error!("axhal supports x86_64 targets only");

#[cfg(test)]
extern crate std;

#[allow(unused_imports)]
#[macro_use]
extern crate log;

#[allow(unused_imports)]
#[macro_use]
extern crate memory_addr;

cfg_if::cfg_if! {
    if #[cfg(feature = "myplat")] {
        // link the custom platform crate in your application.
    }
    else if #[cfg(all(target_os = "none", feature = "defplat"))] {
        extern crate axplat_x86_pc;
    } else {
        // Link the dummy platform implementation to pass cargo test.
        mod dummy;
    }
}

pub mod dtb;
pub mod mem;
pub mod percpu;
pub mod time;

/// x86_64 hardware performance-monitoring counters.
///
/// This API is deliberately local-CPU only.  A counter lease pins its caller
/// until it is released and never performs a remote MSR access.
#[cfg(feature = "pmu")]
pub mod pmu {
    #[cfg(target_os = "none")]
    pub use axplat_x86_pc::pmu::*;

    // Host builds deliberately expose the same capability-only surface without
    // linking an implementation that could execute privileged MSR operations.
    #[cfg(not(target_os = "none"))]
    mod host {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum Event {
            Cycles,
            Instructions,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum CounterKind {
            Programmable,
            Fixed,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum Error {
            Unsupported,
            Hypervisor,
            NoCounter,
            Busy,
            Migrated,
            Stale,
            Overflowed,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct Capabilities {
            pub version: u8,
            pub programmable_counters: u8,
            pub programmable_width: u8,
            pub event_mask_length: u8,
            pub unavailable_events: u32,
            pub fixed_counters: u8,
            pub fixed_width: u8,
        }
        impl Capabilities {
            pub const fn programmable_mask(self) -> u64 {
                0
            }
            pub const fn fixed_mask(self) -> u64 {
                0
            }
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct CounterLease;
        pub fn capabilities() -> Result<Capabilities, Error> {
            Err(Error::Unsupported)
        }
        pub fn drain_local() -> Result<usize, Error> {
            Err(Error::Unsupported)
        }
        impl CounterLease {
            pub fn acquire(_: Event, _: CounterKind) -> Result<Self, Error> {
                Err(Error::Unsupported)
            }
            pub fn read(&self) -> Result<u64, Error> {
                Err(Error::Unsupported)
            }
            pub fn settle(&self, _: u64) -> Result<u64, Error> {
                Err(Error::Unsupported)
            }
            pub fn release(self) -> Result<(), Error> {
                Err(Error::Unsupported)
            }
        }
        #[cfg(test)]
        mod tests {
            use super::*;
            #[test]
            fn host_pmu_is_an_unsupported_stub() {
                assert_eq!(capabilities(), Err(Error::Unsupported));
                assert_eq!(CounterLease::acquire(Event::Cycles, CounterKind::Fixed), Err(Error::Unsupported));
            }
        }
    }
    #[cfg(not(target_os = "none"))]
    pub use host::*;
}

#[cfg(feature = "tls")]
pub mod tls;

#[cfg(feature = "irq")]
pub mod irq;

#[cfg(feature = "paging")]
pub mod paging;

/// Console input and output.
pub mod console {
    #[cfg(feature = "irq")]
    pub use axplat::console::irq_num;
    pub use axplat::console::{read_bytes, write_bytes};
}

/// CPU power management.
pub mod power {
    #[cfg(feature = "smp")]
    pub use axplat::power::cpu_boot;
    pub use axplat::power::system_off;
}

/// Terminal x86_64 kexec platform operations.
pub mod kexec {
    #[cfg(all(target_os = "none", feature = "defplat"))]
    pub use axplat_x86_pc::kexec::{
        boot_memory_regions, boot_rsdp, fence_pci_bus_mastering, transition,
        transition_assembly_range, transition32, transition32_blob, transition32_entry_range,
    };

    // Keep syscall/parser code type-checkable in host and non-default-platform
    // builds; these paths never reach the terminal transition there.
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub fn boot_memory_regions() -> &'static [(usize, usize)] {
        &[]
    }
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub fn boot_rsdp() -> Option<&'static [u8; 36]> {
        None
    }
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub fn fence_pci_bus_mastering() {}
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub unsafe fn transition(_: usize, _: usize, _: usize, _: usize) -> ! {
        panic!("kexec platform unavailable")
    }
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub fn transition32_blob() -> &'static [u8] {
        &[]
    }
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub fn transition32_entry_range() -> (usize, usize) {
        (0, 0)
    }
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub fn transition_assembly_range() -> (usize, usize) {
        (0, 0)
    }
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub unsafe fn transition32(_: usize, _: usize, _: usize, _: usize, _: usize) -> ! {
        panic!("kexec platform unavailable")
    }
}

/// Trap handling.
pub mod trap {
    pub use axcpu::trap::{IRQ, PAGE_FAULT, PageFaultFlags, register_trap_handler};
}

/// CPU register states for context switching.
///
/// There are two types of context:
///
/// - [`TaskContext`][axcpu::TaskContext]: The context of a task.
/// - [`TrapFrame`][axcpu::TrapFrame]: The context of an interrupt or an exception.
pub mod context {
    pub use axcpu::{AddressSpaceFallbackReason, TaskContext, TrapFrame};
    #[cfg(feature = "pkeys")]
    pub use axcpu::PKRU_DEFAULT;
    #[cfg(feature = "asid-switch-diagnostics")]
    pub use axcpu::{
        AsidSwitchDiagnosticsSnapshot, asid_switch_diagnostics_snapshot,
        reset_asid_switch_diagnostics, set_asid_switch_diagnostics_enabled,
    };
}

pub use axcpu::asm;
#[cfg(feature = "uspace")]
pub use axcpu::uspace;
pub use axplat::init::init_later;
#[cfg(feature = "smp")]
pub use axplat::init::{init_early_secondary, init_later_secondary};

/// Initializes the platform and boot argument.
/// This function should be called as early as possible.
pub fn init_early(cpu_id: usize, arg: usize) {
    dtb::init(arg);
    axplat::init::init_early(cpu_id, arg);
}

/// Gets the number of CPUs running in the system.
///
/// When SMP is disabled, this function always returns 1.
///
/// When SMP is enabled, it's the smaller one between the platform-declared CPU
/// number [`axplat::power::cpu_num`] and the configured maximum CPU number
/// `axconfig::plat::MAX_CPU_NUM`.
///
/// This value is determined during the BSP initialization phase.
pub fn cpu_num() -> usize {
    #[cfg(feature = "smp")]
    {
        use spin::Lazy;

        /// The number of CPUs in the system. Based on the number declared by the
        /// platform crate and limited by the configured maximum CPU number.
        static CPU_NUM: Lazy<usize> = Lazy::new(|| {
            let max_cpu_num = axconfig::plat::MAX_CPU_NUM;
            let plat_cpu_num = axplat::power::cpu_num();
            let cpu_num = plat_cpu_num.min(max_cpu_num);

            info!("CPU number: max = {max_cpu_num}, platform = {plat_cpu_num}, use = {cpu_num}");

            if plat_cpu_num > max_cpu_num {
                warn!(
                    "platform declares more CPUs ({plat_cpu_num}) than configured max \
                     ({max_cpu_num}), only the first {max_cpu_num} CPUs will be used."
                );
            }

            cpu_num
        });

        *CPU_NUM
    }
    #[cfg(not(feature = "smp"))]
    {
        1
    }
}

#[allow(unused_macros)]
macro_rules! addr_of_sym {
    ($e:ident) => {
        $e as *const () as usize
    };
}
pub(crate) use addr_of_sym;
