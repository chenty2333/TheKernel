//! Network sysfs observes the mount's captured network namespace, not a reader's.
use alloc::{borrow::Cow, format, string::String, sync::Arc, vec::Vec};

use axfs_ng_vfs::{FsName, FsNameBuf, VfsError, VfsResult};
use axnet::{DeviceStats, NetRxQueueStats};

use super::{
    ChildNames, DirMaker, DirMapping, NodeOpsMux, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs,
    try_boxed_names,
};
use crate::task::{AsThread, NetworkNamespace};

const COUNTERS: [&str; 8] = [
    "rx_bytes",
    "rx_packets",
    "rx_errors",
    "rx_dropped",
    "tx_bytes",
    "tx_packets",
    "tx_errors",
    "tx_dropped",
];
// Software datapath diagnostics, kept separate from the standard hardware-
// facing netdev counters above. Each value belongs to this interface's
// namespace-local Router::poll / RX-worker path; none represents a NIC queue
// or CPU-affinity measurement. Counter units are events and values are
// cumulative since interface creation.
const RX_QUEUE_COUNTERS: [&str; 8] = [
    "rx_sw_poll_attempts",
    "rx_sw_poll_idle",
    "rx_sw_frames_consumed",
    "rx_sw_frames_delivered",
    "rx_worker_arm_attempts",
    "rx_worker_arm_successes",
    "rx_worker_arm_unavailable",
    "rx_worker_arm_failures",
];
fn counter_text(stats: DeviceStats, name: &str) -> VfsResult<String> {
    let value = match name {
        "rx_bytes" => stats.rx_bytes,
        "rx_packets" => stats.rx_packets,
        "rx_errors" => stats.rx_errors,
        "rx_dropped" => stats.rx_dropped,
        "tx_bytes" => stats.tx_bytes,
        "tx_packets" => stats.tx_packets,
        "tx_errors" => stats.tx_errors,
        "tx_dropped" => stats.tx_dropped,
        _ => return Err(VfsError::NotFound),
    };
    Ok(format!("{value}\n"))
}

fn rx_queue_counter_text(stats: NetRxQueueStats, name: &str) -> VfsResult<String> {
    let value = match name {
        "rx_sw_poll_attempts" => stats.rx_sw_poll_attempts,
        "rx_sw_poll_idle" => stats.rx_sw_poll_idle,
        "rx_sw_frames_consumed" => stats.rx_sw_frames_consumed,
        "rx_sw_frames_delivered" => stats.rx_sw_frames_delivered,
        "rx_worker_arm_attempts" => stats.rx_worker_arm_attempts,
        "rx_worker_arm_successes" => stats.rx_worker_arm_successes,
        "rx_worker_arm_unavailable" => stats.rx_worker_arm_unavailable,
        "rx_worker_arm_failures" => stats.rx_worker_arm_failures,
        _ => return Err(VfsError::NotFound),
    };
    Ok(format!("{value}\n"))
}

struct NetClass {
    fs: Arc<SimpleFs>,
    namespace: Arc<NetworkNamespace>,
}
impl SimpleDirOps for NetClass {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let interfaces = self.namespace.stack().interfaces();
        let mut names = Vec::new();
        names
            .try_reserve(interfaces.len())
            .map_err(|_| VfsError::NoMemory)?;
        for interface in interfaces {
            names.push(Cow::Owned(FsNameBuf::from_vec(
                interface.name.into_bytes(),
            )?));
        }
        try_boxed_names(names.into_iter())
    }
    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let interface = self
            .namespace
            .stack()
            .interfaces()
            .into_iter()
            .find(|interface| interface.name.as_bytes() == name.as_bytes())
            .ok_or(VfsError::NotFound)?;
        let index = interface.index;
        let mut statistics = DirMapping::new();
        for name in COUNTERS {
            let namespace = self.namespace.clone();
            statistics.add(
                name,
                SimpleFile::new_regular(self.fs.clone(), move || {
                    let stats = namespace
                        .stack()
                        .interface_statistics(index)
                        .ok_or(VfsError::NotFound)?;
                    counter_text(stats, name)
                }),
            );
        }
        for name in RX_QUEUE_COUNTERS {
            let namespace = self.namespace.clone();
            statistics.add(
                name,
                SimpleFile::new_regular(self.fs.clone(), move || {
                    let stats = namespace
                        .stack()
                        .net_rx_queue_statistics(index)
                        .ok_or(VfsError::NotFound)?;
                    rx_queue_counter_text(stats, name)
                }),
            );
        }
        let mut device = DirMapping::new();
        device.add(
            "statistics",
            SimpleDir::new_maker(self.fs.clone(), Arc::new(statistics)),
        );
        device.add(
            "ifindex",
            SimpleFile::new_regular(self.fs.clone(), move || Ok(format!("{index}\n"))),
        );
        if axnet::wireless_interfaces()
            .iter()
            .any(|wireless| wireless.ifindex == index)
        {
            device.add(
                "wireless",
                SimpleDir::new_maker(self.fs.clone(), Arc::new(DirMapping::new())),
            );
        }
        Ok(SimpleDir::new_maker(self.fs.clone(), Arc::new(device)).into())
    }
    fn is_cacheable(&self) -> bool {
        false
    }
}

pub(super) fn class_root(fs: Arc<SimpleFs>) -> DirMapping {
    let namespace = axtask::current_may_uninit()
        .and_then(|task| task.try_as_thread().map(|thread| thread.net_ns()))
        .or_else(crate::file::netlink::initial_network_namespace);
    let mut root = DirMapping::new();
    if let Some(namespace) = namespace {
        let maker: DirMaker = SimpleDir::new_maker(
            fs.clone(),
            Arc::new(NetClass {
                fs: fs.clone(),
                namespace,
            }),
        );
        root.add("net", maker);
        root.add(
            "ieee80211",
            SimpleDir::new_maker(fs.clone(), Arc::new(WirelessPhyClass { fs })),
        );
    }
    root
}

struct WirelessPhyClass {
    fs: Arc<SimpleFs>,
}
impl SimpleDirOps for WirelessPhyClass {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let phys = axnet::wireless_interfaces();
        let mut names = Vec::new();
        names
            .try_reserve(phys.len())
            .map_err(|_| VfsError::NoMemory)?;
        let mut seen = alloc::collections::BTreeSet::new();
        for phy in phys {
            let name = format!("phy{}", phy.phy_index);
            if seen.insert(phy.phy_index) {
                names.push(Cow::Owned(FsNameBuf::from_vec(name.into_bytes())?));
            }
        }
        try_boxed_names(names.into_iter())
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let phy = axnet::wireless_interfaces()
            .into_iter()
            .find(|phy| format!("phy{}", phy.phy_index).as_bytes() == name.as_bytes())
            .ok_or(VfsError::NotFound)?;
        let mut entries = DirMapping::new();
        let index = phy.phy_index;
        entries.add(
            "index",
            SimpleFile::new_regular(self.fs.clone(), move || Ok(format!("{index}\n"))),
        );
        let mac = phy.mac_address;
        entries.add(
            "macaddress",
            SimpleFile::new_regular(self.fs.clone(), move || {
                Ok(format!(
                    "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
                    mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                ))
            }),
        );
        Ok(SimpleDir::new_maker(self.fs.clone(), Arc::new(entries)).into())
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counters_have_unsigned_decimal_newline_format_and_no_aliases() {
        let stats = DeviceStats {
            rx_bytes: u64::MAX,
            rx_packets: 3,
            rx_errors: 5,
            rx_dropped: 7,
            tx_bytes: 11,
            tx_packets: 13,
            tx_errors: 17,
            tx_dropped: 19,
        };
        let expected = [u64::MAX, 3, 5, 7, 11, 13, 17, 19];
        for (name, value) in COUNTERS.into_iter().zip(expected) {
            assert_eq!(counter_text(stats, name).unwrap(), format!("{value}\n"));
        }
        assert!(counter_text(stats, "rx_crc_errors").is_err());
    }

    #[test]
    fn software_rx_poll_counters_use_explicit_units_and_no_hardware_aliases() {
        let stats = NetRxQueueStats {
            rx_sw_poll_attempts: u64::MAX,
            rx_sw_poll_idle: 3,
            rx_sw_frames_consumed: 5,
            rx_sw_frames_delivered: 7,
            rx_worker_arm_attempts: 11,
            rx_worker_arm_successes: 13,
            rx_worker_arm_unavailable: 17,
            rx_worker_arm_failures: 19,
        };
        let expected = [u64::MAX, 3, 5, 7, 11, 13, 17, 19];
        for (name, value) in RX_QUEUE_COUNTERS.into_iter().zip(expected) {
            assert_eq!(
                rx_queue_counter_text(stats, name).unwrap(),
                format!("{value}\n")
            );
        }
        assert!(rx_queue_counter_text(stats, "rx_queue0_packets").is_err());
    }
}
