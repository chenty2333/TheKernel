#![no_std]

#[cfg(test)]
#[macro_use]
extern crate std;

#[macro_use]
extern crate log;
#[macro_use]
extern crate axplat;

macro_rules! diagnostic_println {
    ($($arg:tt)*) => {
        crate::console::emergency_diagnostic_print(format_args!("{}\n", format_args!($($arg)*)))
    };
}

mod apic;
mod boot;
mod boot_info;
/// Fleet-wide CET capability probing and commit.
pub mod cet;
pub mod console;
/// x86 logical-CPU identity and immutable topology snapshots.
pub mod cpu;
#[cfg(feature = "hwp")]
pub mod hwp;
mod init;
pub mod kexec;
mod mem;
mod power;
mod time;

pub use boot_info::ModuleInfo;

/// Configure a PCI INTx line without changing the default ISA routing.
#[cfg(feature = "irq")]
pub fn configure_pci_intx(vector: usize) -> bool {
    apic::configure_pci_intx(vector)
}

/// Install the shared IRQ acknowledgement pass that precedes direct handlers.
#[cfg(feature = "irq")]
pub fn register_shared_dispatcher(dispatcher: fn(usize) -> bool) -> bool {
    apic::register_shared_dispatcher(dispatcher)
}

/// Returns immutable copies of all Multiboot module descriptors.
///
/// The module bytes are reserved from the physical allocator before runtime
/// initialization, so consumers may safely map the returned physical ranges
/// read-only for the lifetime of the booted kernel.
pub fn boot_modules() -> impl Iterator<Item = ModuleInfo> + 'static {
    boot_info::get().modules().iter().flatten().copied()
}

#[cfg(feature = "pmu")]
pub mod pmu;

/// Package-scoped PMU discovery and bounded uncore counter ownership.
///
/// This is intentionally separate from the architectural core-PMU state: an
/// uncore register is owned by one package CPU, never by an arbitrary task
/// CPU.
#[cfg(feature = "pmu")]
pub mod perf_uncore;

/// Panther Lake-only PEBS/LBR/Intel-PT primitives used by the perf core.
#[cfg(feature = "pmu")]
pub mod perf_precise_aux;

#[cfg(feature = "smp")]
mod mp;

pub mod config {
    //! Platform configuration module.
    //!
    //! If the `AX_CONFIG_PATH` environment variable is set, it will load the configuration from the specified path.
    //! Otherwise, it will fall back to the `axconfig.toml` file in the current directory and generate the default configuration.
    //!
    //! If the `PACKAGE` field in the configuration does not match the package name, it will panic with an error message.
    axconfig_macros::include_configs!(path_env = "AX_CONFIG_PATH", fallback = "axconfig.toml");
    assert_str_eq!(
        PACKAGE,
        env!("CARGO_PKG_NAME"),
        "`PACKAGE` field in the configuration does not match the Package name. Please check your \
         configuration file."
    );
}

#[cfg(feature = "smp")]
fn current_cpu_id() -> usize {
    cpu::current_logical_cpu_id()
}

unsafe extern "C" fn rust_entry(magic: usize, mbi: usize) {
    if magic != self::boot::MULTIBOOT_BOOTLOADER_MAGIC
        && magic != self::boot::MULTIBOOT2_BOOTLOADER_MAGIC
    {
        panic!("unsupported x86 boot magic {magic:#x}");
    }
    // This function is called before axruntime clears `.bss`, so it may only
    // validate and record raw arguments in the explicitly initialized record.
    unsafe { self::boot::record_early_args(magic, mbi) };
    // The BSP is always logical CPU 0.  Resolving APIC topology here would
    // read BSS state and ACPI data before the owned handoff is finalized.
    axplat::call_main(0, mbi);
}

unsafe extern "C" fn rust_entry_secondary(_magic: usize) {
    #[cfg(feature = "smp")]
    if _magic == self::boot::MULTIBOOT_BOOTLOADER_MAGIC
        || _magic == self::boot::MULTIBOOT2_BOOTLOADER_MAGIC
    {
        axplat::call_secondary_main(current_cpu_id());
    }
}
