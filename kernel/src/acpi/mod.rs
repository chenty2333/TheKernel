//! Kernel ACPI ownership and policy; native transitions are opt-in.
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
pub mod thermal;
static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
static BUTTONS: SpinNoIrq<Vec<String>> = SpinNoIrq::new(Vec::new());

pub fn enabled() -> bool {
    axhal::boot::command_line_value("acpi") == Some("acpica")
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
    if !enabled() || INIT_TRIED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        return;
    }
    #[cfg(target_os = "none")]
    if let Err(status) = initialize() {
        ec::stop();
        native::stop_worker();
        axhal::acpi::restore_static();
        warn!("acpica: initialization failed status={status:#x}; static fallback restored");
    }
}
#[cfg(target_os = "none")]
fn initialize() -> Result<(), Status> {
    if axhal::acpi::rsdp_pointer() == 0 {
        return Err(tk_acpica::SUPPORT);
    }
    native::start_worker()?;
    // SAFETY: allocation, scheduler, IRQ/APIC and owned RSDP are ready; only
    // explicit acpi=acpica allows firmware AML to take hardware ownership.
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
    let fixed = engine
        .install_fixed_power(axhal::acpi::button_event)
        .is_ok();
    let ec_count = ec::install(&engine, &nodes)?;
    engine.initialize_objects()?;
    let osc = engine.platform_osc();
    info!("acpica: platform _OSC status={osc:?}; no native PCIe control requested");
    engine.update_gpes()?;
    ec::activate(&engine)?;
    info!("acpica: installed EC controllers={ec_count}");
    info!(
        "acpica: ready version=20260930 nodes={} devices={} AML-errors={} fixed-button={} \
         method-buttons={} hardware-unverified",
        nodes.len(),
        nodes.iter().filter(|n| n.kind == 6).count(),
        tk_acpica::aml_error_count(),
        fixed,
        BUTTONS.lock().len()
    );
    let thermal = thermal::init(&nodes)?;
    *ENGINE.lock() = Some(engine);
    axhal::acpi::register_off(power_off);
    axhal::acpi::publish_button(fixed || !BUTTONS.lock().is_empty() || thermal);
    Ok(())
}
fn notify(path: &str, value: u32) {
    if value == 0x80 && BUTTONS.lock().iter().any(|p| p == path) {
        axhal::acpi::button_event();
    }
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
