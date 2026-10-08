//! Scan-node allocation, lookup and table teardown from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_node.c` rev 1.217 and
//! `ieee80211_node.h` rev 1.64 (BSD-3-Clause). Copyright (c) 2001 Atsushi
//! Onoe, (c) 2002, 2003 Sam Leffler, Errno Consulting, and (c) 2008 Damien
//! Bergamini.

use alloc::collections::BTreeMap;

use crate::AccessPoint;

pub const NODE_CACHE_SIZE: usize = 512;
pub const INVALID_SEQUENCE: u16 = 0xffff;
pub const TID_COUNT: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NodeLifecycle {
    #[default]
    Cache,
    Bss,
    Collect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    pub access_point: AccessPoint,
    /// Last advertised complete RSN IE retained for association policy.
    pub saved_rsn_ie: alloc::vec::Vec<u8>,
    /// Last advertised complete WPA vendor IE retained for association policy.
    pub saved_wpa_ie: alloc::vec::Vec<u8>,
    pub association_id: u16,
    pub ht_caps: crate::HtCapabilities,
    pub ht_operation: crate::HtOperation,
    pub vht_caps: crate::VhtCapabilities,
    pub vht_operation: crate::VhtOperation,
    pub he_caps: crate::HeCapabilities,
    pub supported_rsn_protocols: u32,
    pub supported_rsn_akms: u32,
    pub beacon_timestamp: [u8; 8],
    pub receive_timestamp: u64,
    pub beacon_interval: u16,
    pub dtim_count: u8,
    pub dtim_period: u8,
    pub erp: u8,
    pub qos: bool,
    pub uapsd: bool,
    pub edca: crate::EdcaState,
    pub rx_sequence: u16,
    pub qos_rx_sequences: [u16; TID_COUNT],
    pub lifecycle: NodeLifecycle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeAllocError {
    CacheFull,
    DuplicateAddress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeTable {
    nodes: BTreeMap<[u8; 6], NodeRecord>,
    pub bss_node: NodeRecord,
    cache_size: usize,
}

impl Default for NodeTable {
    fn default() -> Self {
        Self {
            nodes: BTreeMap::new(),
            bss_node: NodeRecord {
                access_point: AccessPoint::default(),
                saved_rsn_ie: alloc::vec::Vec::new(),
                saved_wpa_ie: alloc::vec::Vec::new(),
                association_id: 0,
                ht_caps: crate::HtCapabilities::default(),
                ht_operation: crate::HtOperation::default(),
                vht_caps: crate::VhtCapabilities::default(),
                vht_operation: crate::VhtOperation::default(),
                he_caps: crate::HeCapabilities::default(),
                supported_rsn_protocols: 0,
                supported_rsn_akms: 0,
                beacon_timestamp: [0; 8],
                receive_timestamp: 0,
                beacon_interval: 0,
                dtim_count: 0,
                dtim_period: 0,
                erp: 0,
                qos: false,
                uapsd: false,
                edca: crate::EdcaState::default(),
                rx_sequence: INVALID_SEQUENCE,
                qos_rx_sequences: [INVALID_SEQUENCE; TID_COUNT],
                lifecycle: NodeLifecycle::Bss,
            },
            cache_size: NODE_CACHE_SIZE,
        }
    }
}

// upstream: ieee80211_node.c ieee80211_alloc_node_helper()
pub fn allocation_available(table: &NodeTable) -> bool {
    table.nodes.len() < table.cache_size
}

// upstream: ieee80211_node.c ieee80211_node_alloc()
pub fn allocate_node_storage() -> NodeRecord {
    setup_empty_node()
}

// upstream: ieee80211_node.c ieee80211_setup_node()
pub fn setup_node(node: &mut NodeRecord, mac_address: [u8; 6]) {
    node.access_point.bssid = mac_address;
    node.access_point.ssid = [0; 32];
    node.access_point.ssid_len = 0;
    node.access_point.association_failures = 0;
    node.association_id = 0;
    node.rx_sequence = INVALID_SEQUENCE;
    node.qos_rx_sequences = [INVALID_SEQUENCE; TID_COUNT];
    node.lifecycle = NodeLifecycle::Cache;
}

fn setup_empty_node() -> NodeRecord {
    NodeRecord {
        access_point: AccessPoint::default(),
        saved_rsn_ie: alloc::vec::Vec::new(),
        saved_wpa_ie: alloc::vec::Vec::new(),
        association_id: 0,
        ht_caps: crate::HtCapabilities::default(),
        ht_operation: crate::HtOperation::default(),
        vht_caps: crate::VhtCapabilities::default(),
        vht_operation: crate::VhtOperation::default(),
        he_caps: crate::HeCapabilities::default(),
        supported_rsn_protocols: 0,
        supported_rsn_akms: 0,
        beacon_timestamp: [0; 8],
        receive_timestamp: 0,
        beacon_interval: 0,
        dtim_count: 0,
        dtim_period: 0,
        erp: 0,
        qos: false,
        uapsd: false,
        edca: crate::EdcaState::default(),
        rx_sequence: INVALID_SEQUENCE,
        qos_rx_sequences: [INVALID_SEQUENCE; TID_COUNT],
        lifecycle: NodeLifecycle::Cache,
    }
}

// upstream: ieee80211_node.c ieee80211_alloc_node()
pub fn alloc_node(
    table: &mut NodeTable,
    mac_address: [u8; 6],
) -> Result<&mut NodeRecord, NodeAllocError> {
    if table.nodes.contains_key(&mac_address) {
        return Err(NodeAllocError::DuplicateAddress);
    }
    if !allocation_available(table) {
        return Err(NodeAllocError::CacheFull);
    }
    let mut node = allocate_node_storage();
    setup_node(&mut node, mac_address);
    table.nodes.insert(mac_address, node);
    Ok(table.nodes.get_mut(&mac_address).expect("inserted node"))
}

// upstream: ieee80211_node.c ieee80211_find_node()
pub fn find_node<'a>(table: &'a NodeTable, mac_address: &[u8; 6]) -> Option<&'a NodeRecord> {
    table.nodes.get(mac_address)
}

pub fn find_node_mut<'a>(
    table: &'a mut NodeTable,
    mac_address: &[u8; 6],
) -> Option<&'a mut NodeRecord> {
    table.nodes.get_mut(mac_address)
}

// upstream: ieee80211_node.c ieee80211_free_node()
pub fn free_node(table: &mut NodeTable, mac_address: &[u8; 6]) -> Option<NodeRecord> {
    table.nodes.remove(mac_address)
}

// upstream: ieee80211_node.c ieee80211_free_allnodes()
pub fn free_all_nodes(table: &mut NodeTable, clear_bss: bool) {
    table.nodes.clear();
    if clear_bss {
        table.bss_node = NodeTable::default().bss_node;
    }
}

impl NodeTable {
    pub fn with_cache_size(cache_size: usize) -> Self {
        let mut table = Self::default();
        table.cache_size = cache_size;
        table
    }
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&[u8; 6], &NodeRecord)> {
        self.nodes.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_sets_source_sequence_sentinels_and_duplicate_keys_are_refused() {
        let mut table = NodeTable::default();
        let mac = [0, 1, 2, 3, 4, 5];
        let node = alloc_node(&mut table, mac).unwrap();
        assert_eq!(node.access_point.bssid, mac);
        assert_eq!(node.rx_sequence, INVALID_SEQUENCE);
        assert_eq!(node.qos_rx_sequences, [INVALID_SEQUENCE; TID_COUNT]);
        assert_eq!(node.lifecycle, NodeLifecycle::Cache);
        assert_eq!(
            alloc_node(&mut table, mac),
            Err(NodeAllocError::DuplicateAddress)
        );
        assert_eq!(find_node(&table, &mac).unwrap().access_point.bssid, mac);
    }

    #[test]
    fn cache_bound_free_and_keep_bss_behaviors_are_deterministic() {
        let mut table = NodeTable::with_cache_size(1);
        let first = [1; 6];
        let _ = alloc_node(&mut table, first).unwrap();
        assert!(!allocation_available(&table));
        assert_eq!(
            alloc_node(&mut table, [2; 6]),
            Err(NodeAllocError::CacheFull)
        );
        assert_eq!(
            free_node(&mut table, &first).unwrap().access_point.bssid,
            first
        );
        assert!(allocation_available(&table));
        table.bss_node.access_point.bssid = [9; 6];
        let _ = alloc_node(&mut table, first).unwrap();
        free_all_nodes(&mut table, false);
        assert!(table.is_empty());
        assert_eq!(table.bss_node.access_point.bssid, [9; 6]);
        let _ = alloc_node(&mut table, [2; 6]).unwrap();
        free_all_nodes(&mut table, true);
        assert!(table.is_empty());
        assert_eq!(table.bss_node.lifecycle, NodeLifecycle::Bss);
    }
}
