//! Power management.
//! N305 SCI/S5 transitions are not validated on hardware; Q35 is the emulated test.

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

    /// Enter firmware-described S5, or retain the legacy safe fallback.
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
            if !enter_s5() {
                unsafe { PortWriteOnly::new(0x604).write(0x2000u16) };
            }
        }

        axcpu::asm::halt();
        // The emergency channel, not `warn!`, because this function is also the
        // kernel's panic exit path, where the logger may be the thing that is
        // already stuck.
        crate::console::emergency_diagnostic_print(format_args!(
            "power-off: {} did not stop this CPU; firmware S5 or legacy fallback did not \
             complete, so the machine stays powered -- use its power button\n",
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

use core::sync::atomic::{AtomicBool, Ordering};

use kspin::SpinNoIrq;
use x86_64::instructions::port::Port;

use crate::acpi::sleep::{FixedPower, parse_fadt, sleep_types};

static FIXED: SpinNoIrq<Option<FixedPower>> = SpinNoIrq::new(None);
static S5: SpinNoIrq<Option<[u8; 2]>> = SpinNoIrq::new(None);
static BUTTON: AtomicBool = AtomicBool::new(false);
static BUTTON_READY: AtomicBool = AtomicBool::new(false);
// Handler ownership is independent of published button/thermal availability.
static STATIC_SCI: AtomicBool = AtomicBool::new(false);

pub(crate) fn init_early() {
    let mut fixed = None;
    crate::acpi::visit_tables(|table| {
        if table.get(..4) == Some(b"FACP") {
            fixed = parse_fadt(table);
        }
    });
    let Some(fixed) = fixed else {
        return;
    };
    let mut types = None;
    if let Some((_, dsdt)) = crate::cpu::table_length_and_bytes(fixed.dsdt)
        && dsdt.get(..4) == Some(b"DSDT")
    {
        types = sleep_types(&dsdt[36..]);
    }
    crate::acpi::visit_tables(|table| {
        if table.get(..4) == Some(b"SSDT") && types.is_none() {
            types = sleep_types(&table[36..]);
        }
    });
    *S5.lock() = types;
    *FIXED.lock() = Some(fixed);
}

/// Status is only latched/acknowledged here; no logging, sync or task signal in IRQ.
#[cfg(feature = "irq")]
fn sci_handler() {
    if let Some(fixed) = *FIXED.lock() {
        for port in [fixed.event_a, fixed.event_b] {
            if port == 0 {
                continue;
            }
            // SAFETY: validated FADT system-I/O block, handled as W1C status.
            unsafe {
                let status: u16 = Port::new(port).read();
                let enable: u16 = Port::new(port + fixed.event_half).read();
                if status & enable & (1 << 8) != 0 {
                    PortWriteOnly::new(port).write(1u16 << 8);
                    BUTTON.store(true, Ordering::Release);
                }
            }
        }
    }
}

pub(crate) fn init_later() {
    let Some(fixed) = *FIXED.lock() else {
        warn!("acpi-power: no supported fixed FADT; retaining legacy power-off fallback");
        return;
    };
    info!(
        "acpi-power: SCI={} event={:#x} control={:#x} fixed-button={} S5={:?}",
        fixed.sci,
        fixed.event_a,
        fixed.control_a,
        fixed.fixed_button,
        *S5.lock()
    );
    #[cfg(feature = "irq")]
    let mut sci_low = true;
    #[cfg(feature = "irq")]
    if let Some(facts) = crate::cpu::apic_facts() {
        if facts.io_apic_count != 1
            || facts.io_apic_gsi_base != 0
            || facts.override_total != facts.overrides().len()
            || facts.overrides().iter().any(|r| {
                r.source == fixed.sci
                    && (r.bus != 0
                        || r.gsi != u32::from(fixed.sci)
                        || !matches!(r.flags, 0 | 0x0d | 0x0f))
            })
        {
            warn!("acpi-power: unsupported SCI interrupt-source routing");
            return;
        }
        if let Some(record) = facts.overrides().iter().find(|r| r.source == fixed.sci) {
            sci_low = record.flags & 3 != 1;
        }
    } else {
        return;
    }
    #[cfg(feature = "irq")]
    if fixed.fixed_button && fixed.sci < 16 && S5.lock().is_some() {
        // SAFETY: validated system-I/O PM1 control register. A firmware-specified
        // enable command is used only if SCI_EN is absent; bounded readback.
        unsafe {
            let control: u16 = Port::new(fixed.control_a).read();
            if control & 1 == 0 {
                if fixed.smi == 0 || fixed.enable == 0 {
                    return;
                }
                PortWriteOnly::new(fixed.smi).write(fixed.enable);
                let mut enabled = false;
                for _ in 0..1_000_000 {
                    let value: u16 = Port::new(fixed.control_a).read();
                    if value & 1 != 0 {
                        enabled = true;
                        break;
                    }
                    core::hint::spin_loop();
                }
                if !enabled {
                    warn!("acpi-power: firmware did not enable SCI");
                    return;
                }
            }
            // This minimal fixed-event owner handles only PWRBTN. Other PM1
            // sources are disabled instead of leaving an unacknowledged SCI.
            for port in [fixed.event_a, fixed.event_b] {
                if port != 0 {
                    PortWriteOnly::new(port + fixed.event_half).write(0u16);
                    PortWriteOnly::new(port).write(1u16 << 8);
                }
            }
        }
        let vector = usize::from(fixed.sci) + 0x20;
        if !crate::apic::configure_sci(vector, sci_low)
            || !axplat::irq::register(vector, sci_handler)
        {
            warn!("acpi-power: SCI routing/registration unavailable");
            return;
        }
        // SAFETY: validated event enable ports, SCI handler registered first.
        unsafe {
            for port in [fixed.event_a, fixed.event_b] {
                if port != 0 {
                    PortWriteOnly::new(port + fixed.event_half).write(1u16 << 8);
                }
            }
        }
        STATIC_SCI.store(true, Ordering::Release);
        BUTTON_READY.store(true, Ordering::Release);
    }
}
/// Whether the fixed-button SCI path is enabled.
pub fn power_button_available() -> bool {
    BUTTON_READY.load(Ordering::Acquire)
}
/// Drain the one-bit coalesced power button event from a task, never IRQ context.
pub fn take_power_button_event() -> bool {
    BUTTON.swap(false, Ordering::AcqRel)
}

fn enter_s5() -> bool {
    let callback=ACPICA_OFF.load(Ordering::Acquire);
    if callback != 0 {
        // SAFETY: immutable fn() -> bool installed after ACPICA is ready.
        let off=unsafe { core::mem::transmute::<usize,fn()->bool>(callback) };
        if off() { return true; }
    }
    let fixed = *FIXED.lock();
    let types = *S5.lock();
    let (Some(fixed), Some(types)) = (fixed, types) else {
        return false;
    };
    axcpu::asm::disable_irqs();
    // SAFETY: early discovery validated both ports and literal sleep types.
    // Preserve SCI_EN/reserved bits, first stage both sleep types without EN,
    // then assert SLP_EN in each populated control block (ACPI 16.1.6).
    unsafe {
        let a: u16 = Port::new(fixed.control_a).read();
        let a = (a & !0x3c00) | (u16::from(types[0]) << 10);
        let b = if fixed.control_b != 0 {
            let b: u16 = Port::new(fixed.control_b).read();
            (b & !0x3c00) | (u16::from(types[1]) << 10)
        } else {
            0
        };
        PortWriteOnly::new(fixed.control_a).write(a);
        if fixed.control_b != 0 {
            PortWriteOnly::new(fixed.control_b).write(b);
        }
        PortWriteOnly::new(fixed.control_a).write(a | (1 << 13));
        if fixed.control_b != 0 {
            PortWriteOnly::new(fixed.control_b).write(b | (1 << 13));
        }
    }
    true
}

// ACPICA takes SCI ownership only after successful OSL registration.
static ACPICA_OFF: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static ACPICA_SCI: AtomicBool = AtomicBool::new(false);
pub fn register_acpica_off(callback:fn()->bool) { ACPICA_OFF.store(callback as usize,Ordering::Release); }
pub fn record_acpica_button() { BUTTON.store(true,Ordering::Release); }
pub fn publish_acpica_button(available:bool) { BUTTON_READY.store(available,Ordering::Release); }
#[cfg(feature="irq")]
pub fn install_acpica_sci(irq:u32,handler:fn())->Option<usize> {
    let fixed=(*FIXED.lock())?;
    if irq!=u32::from(fixed.sci)||fixed.sci>=16{return None;}
    let facts=crate::cpu::apic_facts()?;
    if facts.io_apic_count!=1||facts.io_apic_gsi_base!=0||facts.override_total!=facts.overrides().len()
        ||facts.overrides().iter().any(|r|r.source==fixed.sci&&(r.bus!=0||r.gsi!=irq||!matches!(r.flags,0|0x0d|0x0f))){return None;}
    let low=facts.overrides().iter().find(|r|r.source==fixed.sci).is_none_or(|r|r.flags&3!=1);
    let vector=usize::from(fixed.sci)+0x20;
    if !crate::apic::configure_sci(vector,low){return None;}
    BUTTON_READY.store(false, Ordering::Release);
    if STATIC_SCI.swap(false,Ordering::AcqRel){let _=axplat::irq::unregister(vector);}
    if !axplat::irq::register(vector,handler){return None;}
    ACPICA_SCI.store(true,Ordering::Release);Some(vector)
}
#[cfg(feature = "irq")]
pub fn install_acpi_gsi(irq: u32, level: bool, low_active: bool, handler: fn()) -> Option<usize> {
    if irq < 16 { return None; }
    let facts = crate::cpu::apic_facts()?;
    if facts.io_apic_count != 1
        || facts.io_apic_gsi_base != 0
        || facts.override_total != facts.overrides().len()
        || facts.overrides().iter().any(|entry| entry.gsi == irq || entry.source as u32 == irq)
    {
        return None;
    }
    let vector = usize::try_from(irq).ok()?.checked_add(0x20)?;
    if vector >= 0xef || !crate::apic::configure_acpi_gsi(vector, level, low_active) {
        return None;
    }
    if !axplat::irq::register(vector, handler) { return None; }
    Some(vector)
}
#[cfg(feature = "irq")]
pub fn remove_acpi_gsi(vector: usize) {
    crate::apic::set_enable(vector, false);
    let _ = axplat::irq::unregister(vector);
}
#[cfg(feature="irq")]
pub fn remove_acpica_sci(vector:usize){if ACPICA_SCI.swap(false,Ordering::AcqRel){let _=axplat::irq::unregister(vector);}}
pub fn restore_static_acpi() {
    ACPICA_OFF.store(0, Ordering::Release);
    BUTTON_READY.store(false, Ordering::Release);
    // Termination may disable PM1 even if ACPICA failed before claiming SCI.
    // Revoke only our old static handler, then re-register and re-enable events.
    #[cfg(feature = "irq")]
    if STATIC_SCI.swap(false, Ordering::AcqRel) {
        if let Some(fixed) = *FIXED.lock() {
            let _ = axplat::irq::unregister(usize::from(fixed.sci) + 0x20);
        }
    }
    init_later();
}
