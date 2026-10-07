//! Pre-probe firmware IRQ table and bridge swizzling. No config-line guessing.
use alloc::{format, string::String, vec::Vec};

use kspin::SpinNoIrq;
use tk_acpica::{
    Engine, Node, Status,
    routing::{self, Scope},
};
type Parent = (u8, u8, u8);
type Pending = (String, u8, Option<Parent>);
static BUSES: SpinNoIrq<Vec<Scope>> = SpinNoIrq::new(Vec::new());
pub fn init(engine: &Engine, nodes: &[Node]) -> Result<(), Status> {
    // Match the active IOAPIC interrupt model before asking AML for _PRT.
    match engine.evaluate("\\_PIC", &[1]) {
        Ok(_) | Err(5) => {}
        Err(e) => return Err(e),
    }
    let mut buses = Vec::new();
    let mut pending: Vec<Pending> = Vec::new();
    for n in nodes.iter().filter(|n| n.kind == 6) {
        if matches!(
            engine.hardware_id(&n.path).as_deref(),
            Ok("PNP0A03" | "PNP0A08")
        ) {
            let seg = optional_integer(engine, &format!("{}._SEG", n.path))?;
            let bus = optional_integer(engine, &format!("{}._BBN", n.path))?;
            if seg != 0 || bus > 255 {
                return Err(tk_acpica::SUPPORT);
            }
            pending.try_reserve(1).map_err(|_| tk_acpica::NO_MEMORY)?;
            pending.push((n.path.clone(), bus as u8, None));
        }
    }
    while let Some((path, number, parent)) = pending.pop() {
        if buses.iter().any(|b: &Scope| b.number == number) || buses.len() >= 64 {
            return Err(tk_acpica::BAD_PARAMETER);
        }
        let routes = match engine.pci_routes(&path) {
            Ok(routes) => routes,
            Err(5) if parent.is_some() => Vec::new(),
            Err(e) => return Err(e),
        };
        for route in &routes {
            if route.gsi >= 208
                || buses
                    .iter()
                    .flat_map(|b| &b.routes)
                    .any(|old| old.gsi == route.gsi && old.active_low != route.active_low)
                || routes
                    .iter()
                    .any(|other| other.gsi == route.gsi && other.active_low != route.active_low)
            {
                return Err(tk_acpica::SUPPORT);
            }
        }
        buses.try_reserve(1).map_err(|_| tk_acpica::NO_MEMORY)?;
        buses.push(Scope {
            number,
            parent,
            routes,
        });
        for n in nodes.iter().filter(|n| n.kind == 6) {
            let Some(tail) = n.path.strip_prefix(&format!("{path}.")) else {
                continue;
            };
            if tail.contains('.') {
                continue;
            }
            let Ok(adr) = engine.integer(&format!("{}._ADR", n.path)) else {
                continue;
            };
            if adr >> 16 > 31 || adr & 0xffff > 7 {
                continue;
            }
            let (dev, func) = ((adr >> 16) as u8, (adr & 0xffff) as u8);
            let Ok(header) = super::native::pci_read(number, dev, func, 0xc) else {
                continue;
            };
            if (header >> 16) & 0x7f != 1 {
                continue;
            }
            let nums = super::native::pci_read(number, dev, func, 0x18)?;
            let (primary, secondary, subordinate) =
                (nums as u8, (nums >> 8) as u8, (nums >> 16) as u8);
            if primary != number || secondary == 0 || secondary > subordinate {
                return Err(tk_acpica::BAD_PARAMETER);
            }
            pending.try_reserve(1).map_err(|_| tk_acpica::NO_MEMORY)?;
            pending.push((n.path.clone(), secondary, Some((number, dev, func))));
        }
    }
    if buses.is_empty() {
        return Err(tk_acpica::SUPPORT);
    }
    info!(
        "acpica: PCI IRQ scopes={} _PIC=IOAPIC before initial probe",
        buses.len()
    );
    *BUSES.lock() = buses;
    Ok(())
}
pub fn publish() -> Result<(), Status> {
    if !axhal::pci_firmware_irq::install(resolve) {
        return Err(tk_acpica::ALREADY_EXISTS);
    }
    Ok(())
}
fn optional_integer(engine: &Engine, path: &str) -> Result<u64, Status> {
    match engine.integer(path) {
        Ok(v) => Ok(v),
        Err(5) => Ok(0),
        Err(e) => Err(e),
    }
}
fn resolve(bus: u8, device: u8, function: u8, pin: u8) -> Option<(u32, bool)> {
    let (gsi, low) = routing::resolve(&BUSES.lock(), bus, device, function, pin)?;
    if gsi >= 208 {
        return None;
    }
    info!(
        "acpica: INTx {:02x}:{:02x}.{} pin={} GSI={} low={}",
        bus, device, function, pin, gsi, low
    );
    Some((gsi, low))
}
