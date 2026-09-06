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
/// Immutable boot-module metadata supplied by the selected platform.
pub mod boot {
    /// Returns the physical range of the Multiboot module explicitly marked
    /// `rootfs`, if one was supplied by the bootloader.
    #[cfg(all(target_os = "none", feature = "defplat"))]
    pub fn rootfs_module() -> Option<(usize, usize)> {
        axplat_x86_pc::boot_modules()
            .find(|module| module.is_rootfs())
            .map(|module| module.range())
    }

    /// Host tests have no bootloader-owned physical module.
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub fn rootfs_module() -> Option<(usize, usize)> {
        None
    }
}

/// Fleet-owned CET terminal-handoff support.
pub mod cet {
    #[cfg(all(target_os = "none", feature = "defplat"))]
    pub use axplat_x86_pc::cet::restore_current_boot_baseline_for_kexec;

    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    #[inline]
    pub fn restore_current_boot_baseline_for_kexec() {}
}
pub mod mem;
pub mod percpu;
pub mod time;

/// Experimental Intel Hardware P-state clamp control for the current CPU.
///
/// The platform implementation never enables HWP. It is available only when
/// firmware has already enabled it for the running CPU.
#[cfg(feature = "hwp-uclamp")]
pub mod hwp {
    #[cfg(target_os = "none")]
    pub use axplat_x86_pc::hwp::*;

    #[cfg(not(target_os = "none"))]
    mod host {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum Error {
            Unsupported,
            InvalidClamp,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct Capabilities {
            pub lowest: u8,
            pub highest: u8,
        }

        pub fn init_current() -> Result<(), Error> {
            Err(Error::Unsupported)
        }
        pub fn capabilities() -> Result<Capabilities, Error> {
            Err(Error::Unsupported)
        }
        pub fn apply_current_clamp(_: u16, _: u16) -> Result<(), Error> {
            Err(Error::Unsupported)
        }
        pub fn restore_current_request() -> Result<(), Error> {
            Err(Error::Unsupported)
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn host_hwp_is_an_unsupported_stub() {
                assert_eq!(init_current(), Err(Error::Unsupported));
                assert_eq!(capabilities(), Err(Error::Unsupported));
                assert_eq!(apply_current_clamp(0, 1024), Err(Error::Unsupported));
                assert_eq!(restore_current_request(), Err(Error::Unsupported));
            }
        }
    }

    #[cfg(not(target_os = "none"))]
    pub use host::*;
}

/// x86_64 hardware performance-monitoring counters.
///
/// This API is deliberately local-CPU only. A lease is a linear token; each
/// operation validates the current CPU and never performs a remote MSR access.
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
            Architectural {
                event_select: u64,
                availability_bit: u8,
            },
            Raw {
                event_select: u64,
                core_type: IntelCoreType,
            },
        }
        pub use axplat_x86_pc::pmu::Error;
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum ProductClass {
            PantherLake,
            ArchitecturalOnly,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum IntelCoreType {
            Unknown(u8),
            Atom,
            Core,
        }
        // Keep host stubs layout-compatible with the x86 PMU API. The real
        // limit is CPUID-discovered on the target CPU.
        pub const MAX_COUNTING_GROUP: usize = 64;
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        pub struct CountingConstraints {
            pub pebs_counter_mask: u64,
            pub needs_lbr: bool,
            pub offcore_slots: u8,
            pub needs_topdown: bool,
            pub smt_shared_slots: u8,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct CountingProgram {
            pub event: Event,
            pub cookie: u64,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct CountingPlacement {
            pub cpu: usize,
            pub generation: u64,
            pub slots: [u8; MAX_COUNTING_GROUP],
            pub len: u8,
            pub constraints: CountingConstraints,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct CountingCompletion {
            pub cookie: u64,
            pub generation: u64,
            pub delta: u64,
            pub overflowed: bool,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct SamplingProgram {
            pub event: Event,
            pub period: u64,
            pub count_user: bool,
            pub count_kernel: bool,
            pub cookie: u64,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct PmiSample {
            pub cookie: u64,
            pub period: u64,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct PmiCompletion {
            pub sample: PmiSample,
            pub counter_bit: u64,
            pub generation: u64,
            pub residual: u64,
            pub overflowed: bool,
            pub lost: u64,
            pub ip: u64,
            pub user: bool,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct StopSample {
            pub residual: u64,
            pub overflowed: bool,
            pub lost: bool,
        }
        #[derive(Debug)]
        pub struct SamplingToken;
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
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct CapabilitySnapshot {
            pub capabilities: Capabilities,
            pub family: u8,
            pub model: u8,
            pub core_type: IntelCoreType,
            pub product: ProductClass,
        }
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct PlacementCapabilities {
            pub programmable_mask: u64,
            pub pebs_counter_mask: u64,
            pub lbr: bool,
            pub offcore_slots: u8,
            pub topdown_slots: u8,
            pub smt_shared_slots: u8,
        }
        impl Capabilities {
            pub const fn programmable_mask(self) -> u64 {
                0
            }
            pub const fn fixed_mask(self) -> u64 {
                0
            }
        }
        pub fn capabilities() -> Result<Capabilities, Error> {
            Err(Error::Unsupported)
        }
        pub fn capability_snapshot() -> Result<CapabilitySnapshot, Error> {
            Err(Error::Unsupported)
        }
        pub fn placement_capabilities() -> Result<PlacementCapabilities, Error> {
            Err(Error::Unsupported)
        }
        pub fn architectural_event_supported_fleet(_: u8) -> Result<(), Error> {
            Err(Error::Unsupported)
        }
        pub fn fleet_capability_snapshot(_: usize) -> Result<CapabilitySnapshot, Error> {
            Err(Error::Unsupported)
        }
        pub fn fleet_cpu_count() -> Result<usize, Error> {
            Err(Error::Unsupported)
        }
        pub fn prepare_current() -> Result<(), Error> {
            Err(Error::Unsupported)
        }
        pub fn commit_current() -> bool {
            false
        }
        pub fn abort_current() {}
        pub fn is_active() -> bool {
            false
        }
        pub fn restore_current_baseline() -> Result<(), Error> {
            Err(Error::Unsupported)
        }
        pub fn counting_place_group_local(
            _: u64,
            _: &[CountingProgram],
        ) -> Result<CountingPlacement, Error> {
            Err(Error::Unsupported)
        }
        pub fn counting_place_group_constrained_local(
            _: u64,
            _: &[CountingProgram],
            _: CountingConstraints,
        ) -> Result<CountingPlacement, Error> {
            Err(Error::Unsupported)
        }
        pub fn counting_stop_settle_current(_: u64) -> Result<usize, Error> {
            Err(Error::Unsupported)
        }
        pub fn counting_copy_completion_current(
            _: u64,
            _: &mut [CountingCompletion],
        ) -> Result<usize, Error> {
            Err(Error::Unsupported)
        }
        pub fn counting_release_completed_current(_: u64) -> Result<usize, Error> {
            Err(Error::Unsupported)
        }
        pub fn counting_take_completion_local(
            _: u64,
            _: &mut [CountingCompletion],
        ) -> Result<usize, Error> {
            Err(Error::Unsupported)
        }
        pub fn sampling_arm_local(_: SamplingProgram) -> Result<SamplingToken, Error> {
            Err(Error::Unsupported)
        }
        pub fn sampling_take_pmi() -> Result<Option<(PmiSample, u64)>, Error> {
            Err(Error::Unsupported)
        }
        pub fn sampling_nmi_take_pmi(_: u64, _: bool) -> Result<Option<PmiCompletion>, Error> {
            Err(Error::Unsupported)
        }
        pub fn sampling_nmi_take_pmis(
            _: u64,
            _: bool,
            _: &mut [PmiCompletion],
        ) -> Result<usize, Error> {
            Err(Error::Unsupported)
        }
        pub fn sampling_nmi_stop_settle_current(_: u64) -> Result<Option<PmiCompletion>, Error> {
            Err(Error::Unsupported)
        }
        pub fn sampling_nmi_take_pending_wake_local() -> bool {
            false
        }
        pub fn sampling_nmi_take_completion_local() -> Option<PmiCompletion> {
            None
        }
        pub fn sampling_nmi_take_completions_local(_: &mut [PmiCompletion]) -> usize {
            0
        }
        pub fn sampling_rearm_local(_: u64, _: u64) -> Result<(), Error> {
            Err(Error::Unsupported)
        }
        pub fn sampling_stop_local(_: SamplingToken) -> Result<StopSample, Error> {
            Err(Error::Unsupported)
        }
        pub fn sampling_quiesce_local() -> Result<usize, Error> {
            Err(Error::Unsupported)
        }
        #[cfg(test)]
        mod tests {
            use super::*;
            #[test]
            fn host_pmu_is_an_unsupported_stub() {
                assert_eq!(capabilities(), Err(Error::Unsupported));
                assert!(matches!(
                    counting_place_group_local(1, &[]),
                    Err(Error::Unsupported)
                ));
            }
        }
    }
    #[cfg(not(target_os = "none"))]
    pub use host::*;
}

/// Discovery-bounded package PMUs (uncore, energy and residency sources).
///
/// Unlike the architectural PMU this namespace never supplies a host or VM
/// emulation.  A missing discovery record is an unsupported event source.
#[cfg(feature = "pmu")]
pub mod perf_uncore {
    pub use axplat_x86_pc::perf_uncore::*;
}

/// Panther Lake-only PEBS/LBR/Intel-PT transport primitives.
///
/// This intentionally remains separate from the architectural PMU surface:
/// callers must first establish a committed PMU fleet and then opt into the
/// exact machine-state facility they need. The platform implementation rejects
/// hardware admission on hosts while retaining its pure parsers and types.
#[cfg(feature = "pmu")]
pub mod perf_precise_aux {
    pub use axplat_x86_pc::perf_precise_aux::*;
}

#[cfg(all(test, feature = "pmu", not(target_os = "none")))]
mod host_perf_tests {
    #[test]
    fn shared_platform_surface_does_not_admit_host_hardware() {
        assert_eq!(super::perf_uncore::discovered_pmus().count(), 0);
        assert_eq!(super::perf_uncore::readonly_pmus().count(), 0);
        assert_eq!(
            super::perf_uncore::rapl_power_unit_for_owner(0),
            Err(super::pmu::Error::Unsupported),
        );
        assert_eq!(
            super::perf_precise_aux::precise_ip_admitted(true),
            Err(super::perf_precise_aux::Error::Unsupported),
        );
    }

    #[test]
    fn shared_platform_surface_retains_pure_record_decoding() {
        use super::perf_precise_aux::{PebsFormat, decode_pebs_record};
        let mut bytes = [0u8; PebsFormat::RECORD_BYTES];
        bytes[8..16].copy_from_slice(&0x1234u64.to_le_bytes());
        let record = decode_pebs_record(PebsFormat::PantherCoveBasic, &bytes).unwrap();
        assert_eq!(record.ip, 0x1234);
    }
}

#[cfg(feature = "tls")]
pub mod tls;

#[cfg(feature = "irq")]
pub mod irq;

#[cfg(feature = "paging")]
pub mod paging;

/// Console input and output.
pub mod console {
    #[cfg(all(target_os = "none", feature = "defplat"))]
    pub use axplat_x86_pc::console::{
        diagnostic_available, emergency_diagnostic_print, try_write_diagnostic_bytes,
    };

    /// Whether the selected platform detected a diagnostic transport.
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub fn diagnostic_available() -> bool {
        false
    }

    /// No diagnostic hardware is available on host/custom-platform builds.
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub fn try_write_diagnostic_bytes(_: &[u8]) -> usize {
        0
    }

    /// Host/custom-platform emergency output is deliberately a no-op.
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub fn emergency_diagnostic_print(_: core::fmt::Arguments<'_>) {}

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
        boot_memory_regions, boot_rsdp, copy_transition, copy_transition_blob,
        copy_transition_entry_range, fence_pci_bus_mastering, transition,
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
    /// # Safety
    ///
    /// This fallback never transfers control and always panics. It is unsafe
    /// only to preserve the platform transition API.
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
    pub fn copy_transition_blob() -> &'static [u8] {
        &[]
    }
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    pub fn copy_transition_entry_range() -> (usize, usize) {
        (0, 0)
    }
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    /// # Safety
    ///
    /// This fallback never transfers control and always panics. It is unsafe
    /// only to preserve the platform transition API.
    pub unsafe fn copy_transition(_: usize, _: usize, _: usize, _: usize) -> ! {
        panic!("kexec platform unavailable")
    }
    #[cfg(not(all(target_os = "none", feature = "defplat")))]
    /// # Safety
    ///
    /// This fallback never transfers control and always panics. It is unsafe
    /// only to preserve the platform transition API.
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
    #[cfg(feature = "pkeys")]
    pub use axcpu::PKRU_DEFAULT;
    pub use axcpu::{AddressSpaceFallbackReason, TaskContext, TrapFrame};
    #[cfg(feature = "asid-switch-diagnostics")]
    pub use axcpu::{
        AsidSwitchDiagnosticsSnapshot, asid_switch_diagnostics_snapshot,
        reset_asid_switch_diagnostics, set_asid_switch_diagnostics_enabled,
    };
    #[cfg(feature = "fp-simd")]
    pub use axcpu::{XsaveLayout, XsaveUnavailable, xsave_image_mxcsr_valid};
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
        ///
        /// The initializer must stay side-effect free: never log from it.  The
        /// logger's `current_cpu_id()` path reaches this value through
        /// `is_init_ok()`, so a log statement here re-enters the incomplete
        /// `Lazy` on the same CPU and spins forever.
        static CPU_NUM: Lazy<usize> = Lazy::new(|| {
            let max_cpu_num = axconfig::plat::MAX_CPU_NUM;
            let plat_cpu_num = axplat::power::cpu_num();
            plat_cpu_num.min(max_cpu_num)
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
