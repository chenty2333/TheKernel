//! Current _PRT routes, including already-programmed PCI interrupt links.
use alloc::{format, string::String, vec::Vec};

use crate::{BAD_PARAMETER, Engine, NO_MEMORY, SUPPORT, Status, Value};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub device: u16,
    pub function: u16,
    pub pin: u8,
    pub gsi: u32,
}
pub fn parse(
    value: Value,
    mut link: impl FnMut(&str, u32) -> Result<u32, Status>,
) -> Result<Vec<Route>, Status> {
    let Value::Package(entries) = value else {
        return Err(BAD_PARAMETER);
    };
    let mut routes = Vec::new();
    routes
        .try_reserve_exact(entries.len())
        .map_err(|_| NO_MEMORY)?;
    for entry in entries {
        let Value::Package(v) = entry else {
            return Err(BAD_PARAMETER);
        };
        if v.len() != 4 {
            return Err(BAD_PARAMETER);
        }
        let (Value::Integer(address), Value::Integer(pin), source, Value::Integer(index)) =
            (&v[0], &v[1], &v[2], &v[3])
        else {
            return Err(BAD_PARAMETER);
        };
        if *address > u32::MAX.into()
            || address >> 16 > 31
            || !(address & 0xffff == 0xffff || address & 0xffff <= 7)
            || *pin > 3
            || *index > u32::MAX.into()
        {
            return Err(BAD_PARAMETER);
        }
        let gsi = match source {
            Value::Integer(0) => *index as u32,
            Value::String(path) | Value::Reference(path) => link(
                core::str::from_utf8(path).map_err(|_| BAD_PARAMETER)?,
                *index as u32,
            )?,
            _ => return Err(BAD_PARAMETER),
        };
        let route = Route {
            device: (address >> 16) as u16,
            function: (address & 0xffff) as u16,
            pin: *pin as u8,
            gsi,
        };
        if routes.iter().any(|r: &Route| {
            r.device == route.device && r.function == route.function && r.pin == route.pin
        }) {
            return Err(BAD_PARAMETER);
        }
        routes.push(route);
    }
    Ok(routes)
}
impl Engine {
    pub fn pci_routes(&self, parent: &str) -> Result<Vec<Route>, Status> {
        parse(
            self.evaluate(&format!("{parent}._PRT"), &[])?,
            |source, index| {
                // Nonzero descriptor selection and inactive/unprogrammed links
                // require allocation/_SRS policy; do not guess a routable IRQ.
                if index != 0 {
                    return Err(SUPPORT);
                }
                let path = if source.starts_with('\\') {
                    String::from(source)
                } else {
                    self.resolve(parent, source)?
                };
                if self.hardware_id(&path).as_deref() != Ok("PNP0C0F") {
                    return Err(SUPPORT);
                }
                let r = crate::resources::parse(&self.resources(&path, false)?)?;
                if r.irqs.len() != 1
                    || r.irqs[0].numbers.len() != 1
                    || !r.irqs[0].level
                    || !r.irqs[0].active_low
                {
                    return Err(SUPPORT);
                }
                Ok(r.irqs[0].numbers[0])
            },
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direct_and_link_routes() {
        let entries = Value::Package(alloc::vec![
            Value::Package(alloc::vec![
                Value::Integer(0xffff),
                Value::Integer(0),
                Value::Integer(0),
                Value::Integer(16)
            ]),
            Value::Package(alloc::vec![
                Value::Integer(0x1ffff),
                Value::Integer(1),
                Value::String(b"LNKA".to_vec()),
                Value::Integer(0)
            ])
        ]);
        let r = parse(entries, |s, i| {
            assert_eq!((s, i), ("LNKA", 0));
            Ok(11)
        })
        .unwrap();
        assert_eq!(r[0].gsi, 16);
        assert_eq!(r[1].gsi, 11);
    }
    #[test]
    fn malformed_routes_rejected() {
        let value = Value::Package(alloc::vec![Value::Package(alloc::vec![
            Value::Integer(0xffff),
            Value::Integer(4),
            Value::Integer(0),
            Value::Integer(16)
        ])]);
        assert!(parse(value, |_, _| Ok(0)).is_err());
    }
}
