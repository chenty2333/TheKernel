//! Explicit optional boot IPv4 configuration; an absent address is not a QEMU
//! address or a wildcard subnet route. DHCP configures the interface later.
use axerrno::{AxError, AxResult};
use smoltcp::wire::{Ipv4Address, Ipv4Cidr};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Config {
    pub address: Option<Ipv4Cidr>,
    pub gateway: Option<Ipv4Address>,
}
pub(crate) fn parse(ip: &str, gateway: &str, prefix: u8) -> AxResult<Config> {
    if prefix > 32 {
        return Err(AxError::InvalidInput);
    }
    let address = if ip.is_empty() {
        None
    } else {
        let ip: Ipv4Address = ip.parse().map_err(|_| AxError::InvalidInput)?;
        (!ip.is_unspecified()).then(|| Ipv4Cidr::new(ip, prefix))
    };
    let gateway = if gateway.is_empty() {
        None
    } else {
        let gateway: Ipv4Address = gateway.parse().map_err(|_| AxError::InvalidInput)?;
        (!gateway.is_unspecified()).then_some(gateway)
    };
    if address.is_none() && gateway.is_some() {
        return Err(AxError::InvalidInput);
    }
    Ok(Config { address, gateway })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dhcp_empty_configuration_publishes_neither_address_nor_gateway() {
        assert_eq!(
            parse("", "", 24).unwrap(),
            Config {
                address: None,
                gateway: None
            }
        );
        assert_eq!(
            parse("0.0.0.0", "0.0.0.0", 24).unwrap(),
            Config {
                address: None,
                gateway: None
            }
        );
        assert!(parse("", "10.0.2.2", 24).is_err());
        assert!(parse("garbage", "", 24).is_err());
        assert!(parse("10.0.2.15", "", 33).is_err());
    }
    #[test]
    fn qemu_static_configuration_and_explicit_address_without_default_are_distinct() {
        let static_ip = parse("10.0.2.15", "10.0.2.2", 24).unwrap();
        assert_eq!(
            static_ip.address.unwrap().address(),
            Ipv4Address::new(10, 0, 2, 15)
        );
        assert_eq!(static_ip.gateway, Some(Ipv4Address::new(10, 0, 2, 2)));
        let no_router = parse("192.168.10.15", "", 24).unwrap();
        assert!(no_router.address.is_some());
        assert!(no_router.gateway.is_none());
    }
}
