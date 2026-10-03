//! Model-independent analog pin -> selector/mixer -> output-converter walk.
use alloc::vec::Vec;

use axdriver_base::{DevError, DevResult};
pub trait Verbs {
    fn verb(&mut self, codec: u8, node: u8, operation: u16, payload: u16) -> DevResult<u32>;
}
#[derive(Clone, Debug)]
pub struct Widget {
    pub node: u8,
    pub caps: u32,
    pub pin_caps: u32,
    pub config: u32,
    pub connections: Vec<u8>,
}
impl Widget {
    fn kind(&self) -> u32 {
        (self.caps >> 20) & 15
    }
}
#[derive(Clone, Debug)]
pub struct Route {
    pub codec: u8,
    pub function: u8,
    pub vendor: u32,
    pub path: Vec<Widget>,
}
fn parameter(v: &mut impl Verbs, c: u8, n: u8, p: u16) -> DevResult<u32> {
    v.verb(c, n, 0xf00, p)
}
fn children(v: &mut impl Verbs, c: u8, n: u8) -> DevResult<core::ops::Range<u16>> {
    let value = parameter(v, c, n, 4)?;
    let start = ((value >> 16) & 255) as u16;
    let count = (value & 255) as u16;
    if start + count > 128 {
        return Err(DevError::InvalidParam);
    }
    Ok(start..start + count)
}
fn connections(v: &mut impl Verbs, c: u8, n: u8) -> DevResult<Vec<u8>> {
    let format = parameter(v, c, n, 0x0e)?;
    let count = (format & 127) as usize;
    let long = format & 128 != 0;
    let per = if long { 2 } else { 4 };
    let bits = if long { 16 } else { 8 };
    let mut list = Vec::new();
    for base in (0..count).step_by(per) {
        let value = v.verb(c, n, 0xf02, base as u16)?;
        for index in 0..per.min(count - base) {
            let raw = (value >> (index * bits)) & if long { 65535 } else { 255 };
            let range = raw & if long { 32768 } else { 128 } != 0;
            let target = raw & if long { 32767 } else { 127 };
            if target == 0 || target > 127 {
                return Err(DevError::InvalidParam);
            }
            if range {
                let previous = *list.last().ok_or(DevError::InvalidParam)?;
                if target <= u32::from(previous) {
                    return Err(DevError::InvalidParam);
                }
                for node in previous + 1..=target as u8 {
                    list.push(node);
                }
            } else {
                list.push(target as u8);
            }
            if list.len() > 32 {
                return Err(DevError::Unsupported);
            }
        }
    }
    Ok(list)
}
fn walk(nodes: &[Widget], node: u8, visited: &mut [bool; 256], path: &mut Vec<Widget>) -> bool {
    if visited[usize::from(node)] || path.len() == 32 {
        return false;
    }
    visited[usize::from(node)] = true;
    let Some(widget) = nodes.iter().find(|w| w.node == node) else {
        return false;
    };
    if widget.caps & (1 << 9) != 0 {
        return false;
    }
    path.push(widget.clone());
    if widget.kind() == 0 {
        return true;
    }
    if [2, 3, 4].contains(&widget.kind()) {
        for target in &widget.connections {
            if walk(nodes, *target, visited, path) {
                return true;
            }
        }
    }
    path.pop();
    false
}
pub fn find_route(nodes: &[Widget]) -> Option<Vec<Widget>> {
    // Prefer headphones, then speakers/line out, never a digital pin.
    for device in [2, 1, 0] {
        for pin in nodes.iter().filter(|w| {
            w.kind() == 4
                && w.pin_caps & 16 != 0
                && (w.config >> 20) & 15 == device
                && w.config >> 30 != 1
        }) {
            let mut path = Vec::new();
            if walk(nodes, pin.node, &mut [false; 256], &mut path) {
                return Some(path);
            }
        }
    }
    None
}
pub fn enumerate(v: &mut impl Verbs, present: u16) -> DevResult<Route> {
    for c in 0..15 {
        if present & (1 << c) == 0 {
            continue;
        }
        let vendor = parameter(v, c, 0, 0)?;
        for function in children(v, c, 0)? {
            let f = function as u8;
            if parameter(v, c, f, 5)? & 255 != 1 {
                continue;
            }
            let mut nodes = Vec::new();
            for node in children(v, c, f)? {
                let n = node as u8;
                let caps = parameter(v, c, n, 9)?;
                let pin = (caps >> 20) & 15 == 4;
                nodes.push(Widget {
                    node: n,
                    caps,
                    pin_caps: if pin { parameter(v, c, n, 0xc)? } else { 0 },
                    config: if pin { v.verb(c, n, 0xf1c, 0)? } else { 0 },
                    connections: if caps & (1 << 8) != 0 {
                        connections(v, c, n)?
                    } else {
                        Vec::new()
                    },
                });
            }
            if let Some(path) = find_route(&nodes) {
                return Ok(Route {
                    codec: c,
                    function: f,
                    vendor,
                    path,
                });
            }
        }
    }
    Err(DevError::Unsupported)
}
fn unmute(
    v: &mut impl Verbs,
    c: u8,
    node: u8,
    input: bool,
    index: u16,
    function: u8,
    override_caps: bool,
) -> DevResult {
    let caps = parameter(
        v,
        c,
        if override_caps { node } else { function },
        if input { 0xd } else { 0x12 },
    )?;
    let gain = (caps & 127).min((caps >> 8) & 127) as u16;
    v.verb(
        c,
        node,
        0x300,
        (if input { 0x4000 } else { 0x8000 }) | 0x3000 | (index << 8) | gain,
    )?;
    Ok(())
}
pub fn configure(v: &mut impl Verbs, route: &Route) -> DevResult {
    let c = route.codec;
    v.verb(c, route.function, 0x705, 0)?;
    for (index, w) in route.path.iter().enumerate() {
        if w.caps & (1 << 10) != 0 {
            v.verb(c, w.node, 0x705, 0)?;
        }
        if let Some(next) = route.path.get(index + 1) {
            let selected = w
                .connections
                .iter()
                .position(|n| *n == next.node)
                .ok_or(DevError::BadState)?;
            if w.kind() != 2 && w.connections.len() > 1 {
                v.verb(c, w.node, 0x701, selected as u16)?;
            }
            if w.caps & 2 != 0 {
                for i in 0..w.connections.len() {
                    if i != selected {
                        v.verb(c, w.node, 0x300, 0x7080 | ((i as u16) << 8))?;
                    }
                }
                unmute(
                    v,
                    c,
                    w.node,
                    true,
                    selected as u16,
                    route.function,
                    w.caps & 8 != 0,
                )?;
            }
        }
        if w.caps & 4 != 0 {
            unmute(v, c, w.node, false, 0, route.function, w.caps & 8 != 0)?;
        }
        if w.kind() == 4 {
            v.verb(
                c,
                w.node,
                0x707,
                0x40 | if w.pin_caps & 8 != 0 { 0x80 } else { 0 },
            )?;
            if w.pin_caps & (1 << 16) != 0 {
                v.verb(c, w.node, 0x70c, 2)?;
            }
        }
        if w.kind() == 0 {
            v.verb(c, w.node, 0x200, crate::desc::FORMAT)?;
            v.verb(c, w.node, 0x706, 0x10)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct InvalidNodes;
    impl Verbs for InvalidNodes {
        fn verb(&mut self, _: u8, _: u8, operation: u16, payload: u16) -> DevResult<u32> {
            Ok(match (operation, payload) {
                (0xf00, 4) => 0x7f0002,
                (0xf00, 0x0e) => 0x81,
                (0xf02, _) => 128,
                _ => 0,
            })
        }
    }
    #[test]
    fn indirect_node_bit_is_never_used_as_an_eighth_node_bit() {
        assert!(children(&mut InvalidNodes, 0, 0).is_err());
        assert!(connections(&mut InvalidNodes, 0, 2).is_err());
    }
}
