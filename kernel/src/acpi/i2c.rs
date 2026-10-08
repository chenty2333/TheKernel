//! ACPICA resource provider for default-enabled I2C controllers.
use alloc::{borrow::ToOwned, format, string::String, vec::Vec};

use axdriver::i2c::{AcpiGpioPolarity, AcpiI2cChild, AcpiI2cGpioInterrupt, AcpiI2cSupport};
use tk_acpica::{Engine, Node, Value};

// upstream: iichid.c acpi_is_iichid()
// upstream: iichid.c iichid_get_config_reg()
fn engine_child_devices(engine: &Engine, nodes: &[Node], controller: &str) -> Vec<AcpiI2cChild> {
    let mut children = Vec::new();
    const I2C_HID_DSM_UUID: [u8; 16] = [
        0xf7, 0xf6, 0xdf, 0x3c, 0x67, 0x42, 0x55, 0x45, 0xad, 0x05, 0xb3, 0x0a, 0x3d, 0x89, 0x38,
        0xde,
    ];
    for node in nodes.iter().filter(|node| node.kind == 6) {
        if engine
            .integer(&format!("{}._STA", node.path))
            .is_ok_and(|status| status & 1 == 0)
        {
            continue;
        }
        let hid = match engine.hardware_id(&node.path) {
            Ok(hid) if matches!(hid.as_str(), "PNP0C50" | "ACPI0C50" | "ELAN0000") => hid,
            _ => continue,
        };
        let descriptor_register = if hid == "ELAN0000" {
            // FreeBSD's ELAN quirk uses a fixed descriptor register and skips _DSM.
            0x0001
        } else {
            let dsm_path = format!("{}._DSM", node.path);
            let Some(register) = engine
                .evaluate_dsm_integer(&dsm_path, &I2C_HID_DSM_UUID, 1, 1)
                .ok()
                .map(|value| value as u16)
            else {
                warn!(
                    "acpica: I2C HID {} _DSM descriptor address unavailable",
                    node.path
                );
                continue;
            };
            register
        };
        let resources = match engine.resources(&node.path, false) {
            Ok(resources) => resources,
            Err(status) => {
                warn!("acpica: I2C HID {} _CRS failed {status:#x}", node.path);
                continue;
            }
        };
        let buses = match tk_acpica::resources::parse_i2c_serial_buses(&resources) {
            Ok(buses) => buses,
            Err(status) => {
                warn!("acpica: I2C HID {} _CRS malformed {status:#x}", node.path);
                continue;
            }
        };
        let gpio_interrupts = match tk_acpica::resources::parse_gpio_interrupts(&resources) {
            Ok(resources) => resources
                .into_iter()
                .filter_map(|resource| {
                    let controller_path = match core::str::from_utf8(&resource.resource_source) {
                        Ok(path) => path.to_owned(),
                        Err(_) => {
                            warn!(
                                "acpica: I2C HID {} has non-UTF8 GPIO ResourceSource",
                                node.path
                            );
                            return None;
                        }
                    };
                    Some(AcpiI2cGpioInterrupt {
                        controller_path,
                        source_index: resource.source_index,
                        pins: resource.pins,
                        edge_triggered: resource.trigger == tk_acpica::resources::GpioTrigger::Edge,
                        polarity: match resource.polarity {
                            tk_acpica::resources::GpioPolarity::ActiveHigh => {
                                AcpiGpioPolarity::ActiveHigh
                            }
                            tk_acpica::resources::GpioPolarity::ActiveLow => {
                                AcpiGpioPolarity::ActiveLow
                            }
                            tk_acpica::resources::GpioPolarity::Both => AcpiGpioPolarity::Both,
                        },
                        shared: resource.shared,
                        wake_capable: resource.wake_capable,
                        debounce_timeout_us: resource.debounce_timeout_us,
                        pin_config: resource.pin_config,
                    })
                })
                .collect::<Vec<_>>(),
            Err(status) => {
                warn!(
                    "acpica: I2C HID {} GPIO resource decode failed {status:#x}",
                    node.path
                );
                Vec::new()
            }
        };
        for bus in buses {
            let Ok(source) = core::str::from_utf8(&bus.resource_source) else {
                warn!(
                    "acpica: I2C HID {} has a non-UTF8 ResourceSource",
                    node.path
                );
                continue;
            };
            if source != controller {
                continue;
            }
            children.push(AcpiI2cChild {
                path: node.path.clone(),
                hid: hid.clone(),
                slave_address: bus.slave_address,
                ten_bit: bus.ten_bit,
                speed_hz: bus.connection_speed_hz,
                hid_descriptor_register: Some(descriptor_register),
                gpio_interrupts: gpio_interrupts.clone(),
            });
        }
    }
    children
}

fn direct_pci_companion(
    engine: &Engine,
    nodes: &[Node],
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
) -> Option<String> {
    for root in nodes.iter().filter(|node| node.kind == 6) {
        if !matches!(
            engine.hardware_id(&root.path).as_deref(),
            Ok("PNP0A03" | "PNP0A08")
        ) {
            continue;
        }
        let root_segment = engine.integer(&format!("{}._SEG", root.path)).unwrap_or(0);
        let root_bus = engine.integer(&format!("{}._BBN", root.path)).unwrap_or(0);
        if root_segment != u64::from(segment) || root_bus != u64::from(bus) {
            continue;
        }
        let prefix = format!("{}.", root.path);
        for candidate in nodes.iter().filter(|node| node.kind == 6) {
            let Some(tail) = candidate.path.strip_prefix(&prefix) else {
                continue;
            };
            if tail.contains('.') {
                continue;
            }
            let Ok(adr) = engine.integer(&format!("{}._ADR", candidate.path)) else {
                continue;
            };
            if adr >> 16 == u64::from(device) && adr & 0xffff == u64::from(function) {
                return Some(candidate.path.clone());
            }
        }
    }
    None
}

struct AcpiI2cServices;

#[crate_interface::impl_interface]
impl AcpiI2cSupport for AcpiI2cServices {
    fn controller_path(segment: u16, bus: u8, device: u8, function: u8) -> Option<String> {
        crate::acpi::with_engine(|engine| {
            let nodes = engine.namespace().ok()?;
            direct_pci_companion(engine, &nodes, segment, bus, device, function)
        })
        .flatten()
    }
    fn enumerate_children(controller_path: &str) -> Vec<AcpiI2cChild> {
        crate::acpi::with_engine(|engine| {
            let Ok(nodes) = engine.namespace() else {
                return Vec::new();
            };
            engine_child_devices(engine, &nodes, controller_path)
        })
        .unwrap_or_default()
    }
    fn clock_params(controller_path: &str, method: &str) -> Option<[u64; 3]> {
        crate::acpi::with_engine(|engine| {
            let path = format!("{controller_path}.{method}");
            let Value::Package(values) = engine.evaluate(&path, &[]).ok()? else {
                return None;
            };
            if values.len() != 3 {
                return None;
            }
            let mut params = [0; 3];
            for (index, value) in values.iter().enumerate() {
                let Value::Integer(value) = value else {
                    return None;
                };
                params[index] = *value;
            }
            Some(params)
        })
        .flatten()
    }
}
