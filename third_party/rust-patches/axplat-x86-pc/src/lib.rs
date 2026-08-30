#![no_std]

#[cfg(test)]
#[macro_use]
extern crate std;

#[macro_use]
extern crate log;
#[macro_use]
extern crate axplat;

mod apic;
mod boot;
mod boot_info;
mod console;
mod cpu;
mod init;
pub mod kexec;
mod mem;
mod power;
mod time;

#[cfg(feature = "pmu")]
pub mod pmu;

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
