use alloc::{boxed::Box, sync::Arc};
use core::{
    pin::Pin,
    task::{Context, Waker},
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::time::{NANOS_PER_MICROS, TimeValue, wall_time_nanos};
use axtask::future::sleep_until;
use smoltcp::{
    iface::{Interface, SocketSet},
    time::Instant,
    wire::{HardwareAddress, IpAddress, IpListenEndpoint},
};

use crate::{
    router::{Router, Rule},
    wrapper::SocketSetWrapper,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct OutboundRoute {
    pub(crate) src_addr: IpAddress,
    pub(crate) device_mask: u64,
}

fn now() -> Instant {
    Instant::from_micros_const((wall_time_nanos() / NANOS_PER_MICROS) as i64)
}

pub struct Service {
    pub iface: Interface,
    pub(crate) router: Router,
    pub(crate) socket_set: Arc<SocketSetWrapper<'static>>,
    timeout: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}
impl Service {
    pub(crate) fn new(mut router: Router, socket_set: Arc<SocketSetWrapper<'static>>) -> Self {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let iface = Interface::new(config, &mut router, now());

        Self {
            iface,
            router,
            socket_set,
            timeout: None,
        }
    }

    pub fn poll(&mut self, sockets: &mut SocketSet) -> bool {
        let timestamp = now();

        self.router.poll(timestamp);
        self.iface.poll(timestamp, &mut self.router, sockets);
        self.router.dispatch(timestamp)
    }

    fn route_for(&self, dst_addr: &IpAddress) -> AxResult<&Rule> {
        self.router
            .table
            .lookup(dst_addr)
            .ok_or_else(|| AxError::from(LinuxError::ENETUNREACH))
    }

    fn device_mask(rule: &Rule) -> u64 {
        if rule.dev >= 64 {
            u64::MAX
        } else {
            1u64 << rule.dev
        }
    }

    pub(crate) fn resolve_outbound(
        &self,
        dst_addr: &IpAddress,
        bound_src: Option<IpAddress>,
    ) -> AxResult<OutboundRoute> {
        self.resolve_outbound_with_dont_route(dst_addr, bound_src, false)
    }

    pub(crate) fn resolve_outbound_with_dont_route(
        &self,
        dst_addr: &IpAddress,
        bound_src: Option<IpAddress>,
        dont_route: bool,
    ) -> AxResult<OutboundRoute> {
        let rule = self.route_for(dst_addr)?;
        if dont_route && rule.via.is_some() {
            return Err(AxError::from(LinuxError::ENETUNREACH));
        }
        if let Some(bound_src) = bound_src {
            if bound_src != rule.src {
                return Err(AxError::from(LinuxError::EADDRNOTAVAIL));
            }
        }
        Ok(OutboundRoute {
            src_addr: bound_src.unwrap_or(rule.src),
            device_mask: Self::device_mask(rule),
        })
    }

    pub(crate) fn validate_bind_addr(&self, addr: IpAddress) -> AxResult<()> {
        if addr.is_unspecified() {
            return Ok(());
        }
        self.resolve_outbound(&addr, Some(addr)).map_or_else(
            |err| {
                if err == AxError::from(LinuxError::ENETUNREACH) {
                    Err(AxError::from(LinuxError::EADDRNOTAVAIL))
                } else {
                    Err(err)
                }
            },
            |_| Ok(()),
        )
    }

    pub fn device_mask_for(&self, endpoint: &IpListenEndpoint) -> u64 {
        match endpoint.addr {
            Some(addr) => self
                .resolve_outbound(&addr, Some(addr))
                .map_or(0, |route| route.device_mask),
            None => u64::MAX,
        }
    }

    pub fn register_waker(&mut self, mask: u64, waker: &Waker) {
        let next = self.iface.poll_at(now(), &self.socket_set.inner.lock());

        if let Some(t) = next {
            let next = TimeValue::from_micros(t.total_micros() as _);

            // drop old timeout future
            self.timeout = None;

            let mut fut = Box::pin(sleep_until(next));
            let mut cx = Context::from_waker(waker);

            if fut.as_mut().poll(&mut cx).is_ready() {
                waker.wake_by_ref();
                return;
            } else {
                self.timeout = Some(fut);
            }
        }

        for (i, device) in self.router.devices.iter().enumerate() {
            if i >= 64 || mask & (1u64 << i) != 0 {
                device.register_waker(waker);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use smoltcp::wire::{IpAddress, Ipv4Address, Ipv4Cidr};

    use super::*;
    use crate::{device::LoopbackDevice, listen_table::ListenTable};

    fn test_service() -> Service {
        let socket_set = Arc::new(SocketSetWrapper::new());
        let listen_table = Arc::new(ListenTable::new());
        let mut router = Router::new(listen_table);
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        router.add_rule(Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            None,
            dev,
            Ipv4Address::new(10, 0, 2, 15).into(),
        ));
        Service::new(router, socket_set)
    }

    #[test]
    fn resolve_outbound_rejects_mismatched_bound_source() {
        let service = test_service();
        let err = service
            .resolve_outbound(
                &IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8)),
                Some(IpAddress::Ipv4(Ipv4Address::LOCALHOST)),
            )
            .unwrap_err();
        assert_eq!(err, AxError::from(LinuxError::EADDRNOTAVAIL));
    }

    #[test]
    fn resolve_outbound_reports_missing_route() {
        let socket_set = Arc::new(SocketSetWrapper::new());
        let listen_table = Arc::new(ListenTable::new());
        let service = Service::new(Router::new(listen_table), socket_set);
        let err = service
            .resolve_outbound(&IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8)), None)
            .unwrap_err();
        assert_eq!(err, AxError::from(LinuxError::ENETUNREACH));
    }

    #[test]
    fn dont_route_rejects_gateway_routes_but_accepts_direct_routes() {
        let socket_set = Arc::new(SocketSetWrapper::new());
        let listen_table = Arc::new(ListenTable::new());
        let mut router = Router::new(listen_table);
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let source = IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 15));
        let destination = IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8));
        router.add_rule(Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2))),
            dev,
            source,
        ));
        let service = Service::new(router, socket_set);

        assert!(
            service
                .resolve_outbound_with_dont_route(&destination, None, false)
                .is_ok()
        );
        assert_eq!(
            service
                .resolve_outbound_with_dont_route(&destination, None, true)
                .unwrap_err(),
            AxError::from(LinuxError::ENETUNREACH)
        );

        let direct = test_service();
        assert!(
            direct
                .resolve_outbound_with_dont_route(&destination, None, true)
                .is_ok()
        );
    }

    #[test]
    fn validate_bind_addr_accepts_local_source() {
        let service = test_service();
        assert_eq!(
            service.validate_bind_addr(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 15))),
            Ok(())
        );
    }

    #[test]
    fn validate_bind_addr_rejects_nonlocal_source() {
        let service = test_service();
        let err = service
            .validate_bind_addr(IpAddress::Ipv4(Ipv4Address::LOCALHOST))
            .unwrap_err();
        assert_eq!(err, AxError::from(LinuxError::EADDRNOTAVAIL));
    }

    #[test]
    fn validate_bind_addr_maps_missing_route_to_not_available() {
        let socket_set = Arc::new(SocketSetWrapper::new());
        let listen_table = Arc::new(ListenTable::new());
        let service = Service::new(Router::new(listen_table), socket_set);
        let err = service
            .validate_bind_addr(IpAddress::Ipv4(Ipv4Address::new(10, 255, 254, 253)))
            .unwrap_err();
        assert_eq!(err, AxError::from(LinuxError::EADDRNOTAVAIL));
    }
}
