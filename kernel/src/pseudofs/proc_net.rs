//! Network proc views select the target task's namespace at child lookup and
//! retain it for the opened file. Linux 7.2.3 proc_net.c/fib_trie.c format facts.
use alloc::{
    borrow::Cow,
    format,
    string::{String, ToString},
    sync::Arc,
};
use core::fmt::Write;

use axfs_ng_vfs::{FsName, NodeType, VfsError, VfsResult};
use axnet::{InterfaceInfo, IpAddress, IpCidr, Ipv4Address, RouteInfo};
use axtask::{AxTaskRef, TaskState, WeakAxTaskRef};

use super::{
    ChildNames, DirMaker, NodeOpsMux, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs,
    try_boxed_names,
};
use crate::task::{AsThread, NetworkNamespace};

const CHILDREN: [&[u8]; 7] = [b"dev", b"route", b"tcp", b"tcp6", b"udp", b"udp6", b"unix"];
const ROUTE_HEADER: &str =
    "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT";

fn dev_snapshot(net_ns: &NetworkNamespace) -> String {
    let mut output = concat!(
        "Inter-|   Receive                                                |  Transmit\n",
        " face |bytes    packets errs drop fifo frame compressed multicast|",
        "bytes    packets errs drop fifo colls carrier compressed\n",
    )
    .to_string();
    for (name, stats) in net_ns.stack().device_stats() {
        let _ = writeln!(
            output,
            "{name:>6}: {rx_bytes:>7} {rx_packets:>7} {rx_errors:>4} {rx_dropped:>4} 0 0 0 0 \
             {tx_bytes:>8} {tx_packets:>7} {tx_errors:>4} {tx_dropped:>4} 0 0 0 0",
            rx_bytes = stats.rx_bytes,
            rx_packets = stats.rx_packets,
            rx_errors = stats.rx_errors,
            rx_dropped = stats.rx_dropped,
            tx_bytes = stats.tx_bytes,
            tx_packets = stats.tx_packets,
            tx_errors = stats.tx_errors,
            tx_dropped = stats.tx_dropped,
        );
    }
    output
}

fn ipv4_word(address: Ipv4Address) -> u32 {
    u32::from_le_bytes(address.octets())
}
fn ipv4_route_row(route: &RouteInfo, interface: &str) -> Option<String> {
    let IpCidr::Ipv4(cidr) = route.destination else {
        return None;
    };
    let prefix = cidr.prefix_len();
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let destination = u32::from_be_bytes(cidr.address().octets()) & mask;
    let gateway = match route.gateway {
        Some(IpAddress::Ipv4(gateway)) => ipv4_word(gateway),
        None | Some(IpAddress::Ipv6(_)) => 0,
    };
    let flags = 1 | if route.gateway.is_some() { 2 } else { 0 } | if prefix == 32 { 4 } else { 0 };
    // There are no per-route priority/advmss/window/rtt metrics in the sole
    // routing table. Linux's corresponding unset metrics and legacy counts are 0.
    Some(format!(
        "{interface}\t{:08X}\t{gateway:08X}\t{flags:04X}\t0\t0\t0\t{:08X}\t0\t0\t0",
        u32::from_le_bytes(destination.to_be_bytes()),
        u32::from_le_bytes(mask.to_be_bytes())
    ))
}
fn render_routes(routes: &[RouteInfo], interfaces: &[InterfaceInfo]) -> String {
    let mut out = format!("{ROUTE_HEADER:<127}\n");
    for route in routes {
        let Some(interface) = interfaces
            .iter()
            .find(|interface| interface.index == route.interface_index)
        else {
            continue;
        };
        if let Some(row) = ipv4_route_row(route, &interface.name) {
            let _ = writeln!(out, "{row:<127}");
        }
    }
    out
}
fn routes(namespace: &NetworkNamespace) -> String {
    let stack = namespace.stack();
    // Stable ifindices prevent aliasing after removal; a concurrently removed
    // interface's route is omitted rather than relabelled as a different device.
    render_routes(&stack.routes(), &stack.interfaces())
}
struct NetDir {
    fs: Arc<SimpleFs>,
    task: WeakAxTaskRef,
}
impl NetDir {
    fn namespace(&self) -> VfsResult<Arc<NetworkNamespace>> {
        let task = self
            .task
            .upgrade()
            .filter(|task| task.state() != TaskState::Exited)
            .ok_or(VfsError::NotFound)?;
        Ok(task.as_thread().net_ns())
    }
}
impl SimpleDirOps for NetDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let _namespace = self.namespace()?;
        try_boxed_names(
            CHILDREN
                .into_iter()
                .map(|name| Cow::Borrowed(FsName::new(name))),
        )
    }
    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let namespace = self.namespace()?;
        match name.as_bytes() {
            b"dev" => {
                Ok(
                    SimpleFile::new_regular(self.fs.clone(), move || Ok(dev_snapshot(&namespace)))
                        .into(),
                )
            }
            b"route" => {
                Ok(SimpleFile::new_regular(self.fs.clone(), move || Ok(routes(&namespace))).into())
            }
            b"tcp" | b"tcp6" => {
                let family = if name.as_bytes() == b"tcp" { 2 } else { 10 };
                Ok(SimpleFile::try_new_regular_with_open_credential(self.fs.clone(), move || {
                    super::proc_inet::tcp(&namespace, family)
                })?.into())
            }
            b"udp" | b"udp6" => {
                let family = if name.as_bytes() == b"udp" { 2 } else { 10 };
                Ok(SimpleFile::try_new_regular_with_open_credential(self.fs.clone(), move || {
                    super::proc_inet::udp(&namespace, family)
                })?.into())
            }
            b"unix" => Ok(SimpleFile::try_new_regular_with_open_credential(
                self.fs.clone(), move || super::proc_unix::snapshot(&namespace),
            )?.into()),
            _ => Err(VfsError::NotFound),
        }
    }
    fn is_cacheable(&self) -> bool {
        false
    }
}
pub(super) fn task_dir(fs: Arc<SimpleFs>, task: &AxTaskRef) -> DirMaker {
    SimpleDir::new_maker(
        fs.clone(),
        Arc::new(NetDir {
            fs,
            task: Arc::downgrade(task),
        }),
    )
}
pub(super) fn root_link(fs: Arc<SimpleFs>) -> Arc<SimpleFile> {
    SimpleFile::new(fs, NodeType::Symlink, || Ok("self/net"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn route(address: [u8; 4], prefix: u8, gateway: Option<[u8; 4]>) -> RouteInfo {
        RouteInfo {
            destination: IpCidr::new(Ipv4Address::from(address).into(), prefix),
            gateway: gateway.map(|a| Ipv4Address::from(a).into()),
            interface_index: 2,
            source: Ipv4Address::UNSPECIFIED.into(),
        }
    }
    #[test]
    fn ipv4_fields_have_linux_native_word_order_masks_and_unset_metrics() {
        assert_eq!(
            ipv4_route_row(&route([10, 0, 2, 15], 24, None), "eth0").unwrap(),
            "eth0\t0002000A\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0"
        );
        assert_eq!(
            ipv4_route_row(&route([0, 0, 0, 0], 0, Some([10, 0, 2, 2])), "eth0").unwrap(),
            "eth0\t00000000\t0202000A\t0003\t0\t0\t0\t00000000\t0\t0\t0"
        );
        let mut mixed = route([192, 0, 2, 1], 24, None);
        mixed.gateway = Some(axnet::Ipv6Address::LOCALHOST.into());
        assert!(
            ipv4_route_row(&mixed, "eth0")
                .unwrap()
                .contains("\t00000000\t0003\t")
        );
        assert!(
            ipv4_route_row(&route([127, 0, 0, 1], 32, None), "lo")
                .unwrap()
                .contains("\t0005\t")
        );
    }
    #[test]
    fn route_records_are_padded_and_never_relabel_missing_interface_indices() {
        let interfaces = [InterfaceInfo {
            index: 2,
            name: "eth0".into(),
            kind: axnet::InterfaceKind::Ethernet,
            mtu: 1500,
            administrative_up: true,
            hardware_address: None,
            addresses: alloc::vec![],
        }];
        let table = render_routes(&[route([10, 0, 2, 15], 24, None)], &interfaces);
        assert!(table.lines().all(|line| line.len() == 127));
        assert_eq!(table.lines().count(), 2);
        assert_eq!(
            render_routes(&[route([10, 0, 2, 15], 24, None)], &[])
                .lines()
                .count(),
            1
        );
    }
}
