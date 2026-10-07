//! _PRW registration before automatic GPE enable; no S3 wake-mask ownership.
use alloc::format;

use tk_acpica::{Engine, Node, Status};
pub fn configure(engine: &Engine, nodes: &[Node]) -> Result<usize, Status> {
    let mut count = 0;
    for node in nodes.iter().filter(|n| n.kind == 6) {
        let value = match engine.evaluate(&format!("{}._PRW", node.path), &[]) {
            Ok(value) => value,
            Err(5) => continue, // AE_NOT_FOUND: not a wake-capable device.
            Err(status) => return Err(status),
        };
        let gpe = tk_acpica::gpe::parse(value)?;
        // Power buttons need S0 events even when their GPE is wake-capable.
        let runtime =
            gpe.deepest_state == 0 || engine.hardware_id(&node.path).as_deref() == Ok("PNP0C0C");
        engine.setup_wake_gpe(&node.path, &gpe, runtime)?;
        count += 1;
    }
    Ok(count)
}
