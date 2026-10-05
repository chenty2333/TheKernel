//! Network sysfs observes the mount's captured network namespace, not a reader's.
use alloc::{borrow::Cow, format, string::String, sync::Arc, vec::Vec};

use axfs_ng_vfs::{FsName, FsNameBuf, VfsError, VfsResult};
use axnet::DeviceStats;

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
        let mut device = DirMapping::new();
        device.add(
            "statistics",
            SimpleDir::new_maker(self.fs.clone(), Arc::new(statistics)),
        );
        device.add(
            "ifindex",
            SimpleFile::new_regular(self.fs.clone(), move || Ok(format!("{index}\n"))),
        );
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
        let maker: DirMaker =
            SimpleDir::new_maker(fs.clone(), Arc::new(NetClass { fs, namespace }));
        root.add("net", maker);
    }
    root
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
}
