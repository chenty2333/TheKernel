//! Kernel ACPI ownership and policy; ACPICA is default; static parsing is a minimal rescue.
use alloc::{string::String, vec::Vec};

use axsync::Mutex;
use kspin::SpinNoIrq;
use tk_acpica::{Engine, Node};
#[cfg(target_os = "none")]
use tk_acpica::{Mode, Status};
#[cfg(target_os = "none")]
mod ec;
#[cfg(target_os = "none")]
mod native;
pub mod pchgpio;
#[cfg(target_os = "none")]
mod pci;
pub mod thermal;
#[cfg(any(target_os = "none", test))]
mod vtd;
#[cfg(target_os = "none")]
mod wake;
static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
static BUTTONS: SpinNoIrq<Vec<String>> = SpinNoIrq::new(Vec::new());

fn select_native(option: Option<&str>) -> Result<bool, ()> {
    match option {
        None | Some("acpica") => Ok(true),
        Some("static") => Ok(false),
        Some(_) => Err(()),
    }
}
pub fn enabled() -> bool {
    select_native(axhal::boot::command_line_value("acpi")) == Ok(true)
}
pub fn with_engine<T>(f: impl FnOnce(&Engine) -> T) -> Option<T> {
    let engine = ENGINE.lock();
    engine.as_ref().map(f)
}
pub fn namespace() -> Vec<Node> {
    with_engine(|e| e.namespace().unwrap_or_default()).unwrap_or_default()
}
static INIT_TRIED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
pub fn init() {
    if INIT_TRIED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        return;
    }
    match select_native(axhal::boot::command_line_value("acpi")) {
        Ok(true) => {}
        Ok(false) => {
            info!("acpica: explicit static rescue requested; AML disabled");
            return;
        }
        Err(()) => {
            warn!("acpica: unknown acpi option; using static rescue, AML disabled");
            return;
        }
    }
    #[cfg(target_os = "none")]
    if let Err(status) = initialize() {
        ec::stop();
        native::stop_worker();
        axhal::acpi::restore_static();
        warn!(
            "acpica: initialization failed status={status:#x}; static fallback restored; \
             fixed-button={}",
            axhal::power::power_button_available()
        );
        axhal::console::write_tty_bytes(b"THEKERNEL_ACPICA_INIT_FAILED_STATIC_RESCUE\n");
    }
}
#[cfg(target_os = "none")]
fn initialize() -> Result<(), Status> {
    if axhal::acpi::rsdp_pointer() == 0 {
        return Err(tk_acpica::SUPPORT);
    }
    native::start_worker()?;
    // SAFETY: the pre-probe BSP service boundary established allocation,
    // blocking scheduler, IRQ/timer and owned RSDP before firmware ownership.
    let mut engine = unsafe {
        Engine::initialize_with_tables(&native::REGISTRATION, Mode::Hardware, ec::bootstrap)
    }?;
    engine.install_notify(notify)?;
    let nodes = engine.namespace()?;
    let mut buttons = Vec::new();
    for n in nodes.iter().filter(|n| n.kind == 6) {
        if engine.hardware_id(&n.path).as_deref() == Ok("PNP0C0C") {
            buttons.try_reserve(1).map_err(|_| tk_acpica::NO_MEMORY)?;
            buttons.push(n.path.clone());
        }
    }
    *BUTTONS.lock() = buttons;
    let fixed = engine.fixed_power_supported();
    if fixed {
        engine.install_fixed_power(axhal::acpi::button_event)?;
    }
    let ec_count = ec::install(&engine, &nodes)?;
    let wake_sources = wake::configure(&engine, &nodes)?;
    info!("acpica: registered wake GPE sources={wake_sources}; sleep wake masks remain disabled");
    engine.initialize_objects()?;
    if let Err(error) = vtd::init(&engine) {
        error!("acpica: VT-d initialization failed: {error:?}");
    }
    let gpio_count = pchgpio::init(&engine, &nodes);
    let osc = engine.platform_osc();
    info!("acpica: platform _OSC status={osc:?}; no native PCIe control requested");
    pci::init(&engine, &nodes)?;
    engine.update_gpes()?;
    ec::activate(&engine)?;
    info!("acpica: installed EC controllers={ec_count}");
    let thermal = thermal::init(&nodes)?;
    pci::publish()?;
    *ENGINE.lock() = Some(engine);
    axhal::acpi::register_off(power_off);
    axhal::acpi::publish_button(fixed || !BUTTONS.lock().is_empty() || thermal);
    info!(
        "acpica: ready version=20260930 nodes={} devices={} gpio-providers={} AML-errors={} \
         fixed-button={} method-buttons={} hardware-unverified",
        nodes.len(),
        nodes.iter().filter(|n| n.kind == 6).count(),
        gpio_count,
        tk_acpica::aml_error_count(),
        fixed,
        BUTTONS.lock().len()
    );
    Ok(())
}
fn notify(path: &str, value: u32) {
    if value == 0x80 && BUTTONS.lock().iter().any(|p| p == path) {
        axhal::acpi::button_event();
    }
}

/// Preserve GPIO pad state at the entry boundary of a future ACPI S3 path.
/// S3 entry itself is not yet enabled by this kernel, so this hook is not
/// presently called by a system suspend operation.
pub fn prepare_s3_gpio() {
    pchgpio::save_all();
}

/// Restore GPIO pads before devices resume after ACPI S3.
pub fn resume_s3_gpio() {
    pchgpio::restore_all();
}

#[cfg(target_os = "none")]
fn power_off() -> bool {
    // Panic/IRQ paths must not execute AML or wait on an interpreter mutex.
    if !axhal::asm::irqs_enabled() || !axtask::can_block_current() {
        return false;
    }
    let Some(guard) = ENGINE.try_lock() else {
        return false;
    };
    let Some(engine) = guard.as_ref() else {
        return false;
    };
    if let Err(s) = engine.prepare_s5() {
        warn!("acpica: S5 prep failed {s:#x}; static fallback");
        return false;
    }
    axhal::console::emergency_diagnostic_print(format_args!(
        "acpica: entering S5 after AML preparation\n"
    ));
    axhal::console::write_tty_bytes(b"THEKERNEL_ACPICA_S5_PREPARED\n");
    axhal::acpi_flush_diagnostics();
    axhal::asm::disable_irqs();
    // SAFETY: ordered shutdown already flushed filesystems and IRQs are off.
    if let Err(s) = unsafe { engine.enter_s5() } {
        axhal::console::emergency_diagnostic_print(format_args!(
            "acpica: S5 entry failed {s:#x}; static fallback\n"
        ));
        return false;
    }
    true
}
#[cfg(target_os = "none")]
fn disable_storming_gpes() {
    if let Some(Ok(())) = with_engine(|e| e.disable_gpes()) {
        warn!("acpica: SCI storm threshold reached; GPEs disabled, fixed events retained");
        native::reenable_sci();
    } else {
        warn!("acpica: SCI storm; delivery stays masked (GPE disable unavailable)");
    }
}

struct FirmwareServices;
#[crate_interface::impl_interface]
impl axruntime::PlatformServices for FirmwareServices {
    fn before_pci_probe() {
        #[cfg(target_os = "none")]
        axhal::console::write_tty_bytes(b"THEKERNEL_PLATFORM_SERVICES_READY\n");
        init();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_is_default_and_only_named_rescue_bypasses_it() {
        assert_eq!(super::select_native(None), Ok(true));
        assert_eq!(super::select_native(Some("acpica")), Ok(true));
        assert_eq!(super::select_native(Some("static")), Ok(false));
        assert_eq!(super::select_native(Some("off")), Err(()));
        assert_eq!(super::select_native(Some("")), Err(()));
    }
}

// I2C PCI companions and I2cSerialBusV2 child enumeration for tk-axdriver.
pub(crate) mod i2c;
