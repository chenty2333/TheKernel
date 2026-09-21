//! Power management.

use axplat::power::PowerIf;
use x86_64::instructions::port::PortWriteOnly;

struct PowerImpl;

#[cfg_attr(target_os = "none", impl_plat_interface)]
impl PowerIf for PowerImpl {
    /// Bootstraps the given CPU core with the given initial stack (in physical
    /// address).
    ///
    /// Where `cpu_id` is the logical CPU ID (0, 1, ..., N-1, N is the number of
    /// CPU cores on the platform).
    #[cfg(feature = "smp")]
    fn cpu_boot(cpu_id: usize, stack_top_paddr: usize) {
        use axplat::mem::pa;
        crate::mp::start_secondary_cpu(cpu_id, pa!(stack_top_paddr))
    }

    /// Power off the whole system.
    ///
    /// The register written below is an emulator affordance.  QEMU's PIIX4
    /// (`pc`) and ICH9 (`q35`) southbridges decode PM1a_CNT at I/O port 0x604
    /// when firmware assigned no other base, and there `SLP_EN` with a sleep
    /// type of zero means S5 -- the same distinction spelled out in
    /// `tools/nested/hello/hello.c`.  On a real machine firmware puts PM1a_CNT
    /// wherever the FADT says, and the S5 encoding comes from the DSDT's `_S5`
    /// object, which this kernel never evaluates; Linux reads both before it
    /// even *offers* the method (`acpi_sleep_state_supported`,
    /// drivers/acpi/sleep.c:87-96, gates the registration of `acpi_power_off`
    /// at :1117-1126).  So on real hardware the write is ignored and the CPU
    /// keeps running.
    ///
    /// Halting is the right thing to do then, and matches Linux when no
    /// power-off handler is registered (`do_kernel_power_off`,
    /// kernel/reboot.c:658-665 -- "Otherwise does nothing" -- falling through
    /// to `machine_halt`).  What must not happen is silence, so the line after
    /// the first `halt` says which method was tried and that the machine is
    /// still powered.
    fn system_off() -> ! {
        info!("Shutting down...");

        // For real hardware platforms, using port `0x604` to shutdown does not
        // work. Therefore we use port `0x64` to reboot the system instead.
        let reboot_instead = cfg!(feature = "reboot-on-system-off");
        if reboot_instead {
            axplat::console_println!("System will reboot, press any key to continue ...");
            while super::console::getchar().is_none() {}
            axplat::console_println!("Rebooting ...");
            crate::console::flush_diagnostic();
            // SAFETY: 0x64 is the 8042 command port; the data byte is the
            // controller's own "pulse the CPU reset line" command.
            unsafe { PortWriteOnly::new(0x64).write(0xfeu8) };
        } else {
            crate::console::flush_diagnostic();
            // SAFETY: 0x604 is a 16-bit I/O port.  A machine that does not
            // decode it drops the write, which is the case reported below.
            unsafe { PortWriteOnly::new(0x604).write(0x2000u16) };
        }

        axcpu::asm::halt();
        // The emergency channel, not `warn!`, because this function is also the
        // kernel's panic exit path, where the logger may be the thing that is
        // already stuck.
        crate::console::emergency_diagnostic_print(format_args!(
            "power-off: {} did not stop this CPU; this kernel evaluates no ACPI _S5 \
             sequence, so the machine stays powered -- use its power button\n",
            if reboot_instead {
                "the 8042 reset command on port 0x64"
            } else {
                "PM1a_CNT SLP_EN on port 0x604"
            }
        ));
        crate::console::flush_diagnostic();
        warn!("power-off: halted while the machine is still powered, see the line above");
        loop {
            axcpu::asm::halt();
        }
    }

    /// Get the number of CPU cores available on this platform.
    fn cpu_num() -> usize {
        crate::cpu::cpu_num()
    }
}

/// Reset the x86 platform rather than using the ACPI power-off port.
pub fn system_reset() -> ! {
    crate::console::flush_diagnostic();
    axcpu::asm::disable_irqs();
    // Q35/ICH reset-control register: assert system reset, then CPU reset.
    unsafe {
        PortWriteOnly::new(0xcf9).write(0x02u8);
        PortWriteOnly::new(0xcf9).write(0x06u8);
        // Legacy 8042 fallback, also supported by the QEMU pc machine.
        PortWriteOnly::new(0x64).write(0xfeu8);
    }
    loop {
        axcpu::asm::halt();
    }
}
