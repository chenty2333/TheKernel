//! ACPICA thermal-zone policy: critical protection only, no cooling governor.
use alloc::{format, string::String, vec::Vec};
use core::time::Duration;

use kspin::SpinNoIrq;
use tk_acpica::{Node, Status};
static ZONES: SpinNoIrq<Vec<String>> = SpinNoIrq::new(Vec::new());
pub fn zones() -> Vec<String> {
    ZONES.lock().clone()
}
pub fn init(nodes: &[Node]) -> Result<bool, Status> {
    let mut zones = Vec::new();
    zones
        .try_reserve_exact(nodes.len())
        .map_err(|_| tk_acpica::NO_MEMORY)?;
    zones.extend(
        nodes
            .iter()
            .filter(|n| n.kind == 13)
            .map(|n| n.path.clone()),
    );
    let available = !zones.is_empty();
    *ZONES.lock() = zones;
    if available {
        axtask::spawn_raw(monitor, "acpi_thermal".into(), axconfig::TASK_STACK_SIZE)
            .map_err(|_| tk_acpica::NO_MEMORY)?;
    }
    Ok(available)
}
pub fn temperature(path: &str, method: &str) -> Result<i64, Status> {
    super::with_engine(|e| {
        let raw = e.integer(&format!("{path}.{method}"))?;
        let critical = e.integer(&format!("{path}._CRT")).ok();
        tk_acpica::thermal::millicelsius(raw, critical)
    })
    .unwrap_or(Err(tk_acpica::SUPPORT))
}
fn monitor() {
    loop {
        for path in zones() {
            let critical = super::with_engine(|e| {
                match (
                    e.integer(&format!("{path}._TMP")),
                    e.integer(&format!("{path}._CRT")),
                ) {
                    (Ok(t), Ok(c)) => tk_acpica::thermal::critical_reached(t, c),
                    _ => false,
                }
            })
            .unwrap_or(false);
            if critical {
                warn!("acpica: thermal critical trip reached; requesting ordered shutdown");
                axhal::acpi::button_event();
                return;
            }
        }
        if axtask::sleep(Duration::from_secs(5)).is_err() {
            return;
        }
    }
}
