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
pub const INACT_SCAN: u8 = 10;

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
    pub inactivity: u8,
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
    pub uapsd_access_categories: u8,
    pub uapsd_max_service_period: u8,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeCopyEffects {
    pub reset_node_timeouts: bool,
    pub delete_block_ack_state: bool,
    pub release_rx_reorder_buffers: bool,
    pub retire_unreference_callback: bool,
    pub reinitialize_hostap_power_save_queue: bool,
}

/// Replace a node's owned station record and request source timeout reset.
// upstream: ieee80211_node.c ieee80211_node_copy()
pub fn copy_node_state(destination: &mut NodeRecord, source: &NodeRecord) -> NodeCopyEffects {
    *destination = source.clone();
    NodeCopyEffects {
        reset_node_timeouts: true,
        delete_block_ack_state: true,
        release_rx_reorder_buffers: true,
        retire_unreference_callback: true,
        reinitialize_hostap_power_save_queue: false,
    }
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
                inactivity: 0,
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
                uapsd_access_categories: 0,
                uapsd_max_service_period: 0,
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
    node.inactivity = 0;
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
        inactivity: 0,
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
        uapsd_access_categories: 0,
        uapsd_max_service_period: 0,
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

/// Allocate an RX peer node and inherit the station BSS identity/channel.
// upstream: ieee80211_node.c ieee80211_dup_bss()
pub fn duplicate_bss_node(
    table: &mut NodeTable,
    mac_address: [u8; 6],
) -> Result<&mut NodeRecord, NodeAllocError> {
    let bssid = table.bss_node.access_point.bssid;
    let channel = table.bss_node.access_point.channel;
    let node = alloc_node(table, mac_address)?;
    node.access_point.bssid = bssid;
    node.access_point.channel = channel;
    Ok(node)
}

// upstream: ieee80211_node.c ieee80211_find_node()
pub fn find_node<'a>(table: &'a NodeTable, mac_address: &[u8; 6]) -> Option<&'a NodeRecord> {
    table.nodes.get(mac_address)
}

/// Return the station BSS peer for every STA TX destination or multicast frame.
// upstream: ieee80211_node.c ieee80211_find_txnode()
pub fn find_station_tx_node<'a>(
    table: &'a NodeTable,
    station_mode: bool,
    destination: [u8; 6],
) -> Option<&'a NodeRecord> {
    if station_mode || destination[0] & 1 != 0 {
        Some(&table.bss_node)
    } else {
        None
    }
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

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&[u8; 6], &mut NodeRecord)> {
        self.nodes.iter_mut()
    }
}

/// Increase the station-scan inactivity age only when no packet owner holds a reference.
// upstream: ieee80211_node.c ieee80211_node_raise_inact()
pub fn raise_scan_node_inactivity(node: &mut NodeRecord, referenced: bool) {
    if !referenced && node.inactivity < INACT_SCAN {
        node.inactivity += 1;
    }
}

/// Remove unreferenced node-cache entries that have reached the selected inactivity age.
// upstream: ieee80211_node.c ieee80211_clean_inactive_nodes()
pub fn clean_inactive_nodes(
    table: &mut NodeTable,
    inactivity_limit: u8,
    mut referenced: impl FnMut(&[u8; 6]) -> bool,
) -> usize {
    let before = table.nodes.len();
    table
        .nodes
        .retain(|mac, node| referenced(mac) || node.inactivity < inactivity_limit);
    before - table.nodes.len()
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

    #[test]
    fn station_scan_ages_unreferenced_nodes_and_cleans_only_expired_entries() {
        let mut table = NodeTable::default();
        let expired = [0, 0, 0, 0, 0, 1];
        let referenced = [0, 0, 0, 0, 0, 2];
        let young = [0, 0, 0, 0, 0, 3];
        alloc_node(&mut table, expired).unwrap().inactivity = INACT_SCAN;
        alloc_node(&mut table, referenced).unwrap().inactivity = INACT_SCAN;
        alloc_node(&mut table, young).unwrap().inactivity = INACT_SCAN - 2;
        let node = find_node_mut(&mut table, &young).unwrap();
        raise_scan_node_inactivity(node, false);
        assert_eq!(node.inactivity, INACT_SCAN - 1);
        raise_scan_node_inactivity(node, true);
        assert_eq!(node.inactivity, INACT_SCAN - 1);
        let node = find_node_mut(&mut table, &referenced).unwrap();
        raise_scan_node_inactivity(node, true);
        assert_eq!(node.inactivity, INACT_SCAN);

        let removed = clean_inactive_nodes(&mut table, INACT_SCAN, |mac| *mac == referenced);
        assert_eq!(removed, 1);
        assert!(find_node(&table, &expired).is_none());
        assert!(find_node(&table, &referenced).is_some());
        assert!(find_node(&table, &young).is_some());
    }

    #[test]
    fn node_copy_owns_saved_information_elements_and_requests_timeout_reset() {
        let mut source = setup_empty_node();
        source.saved_rsn_ie = alloc::vec![48, 1, 7];
        let mut destination = setup_empty_node();
        destination.saved_wpa_ie = alloc::vec![221, 1, 9];
        let effects = copy_node_state(&mut destination, &source);
        assert_eq!(destination.saved_rsn_ie, [48, 1, 7]);
        assert!(destination.saved_wpa_ie.is_empty());
        assert_eq!(
            effects,
            NodeCopyEffects {
                reset_node_timeouts: true,
                delete_block_ack_state: true,
                release_rx_reorder_buffers: true,
                retire_unreference_callback: true,
                reinitialize_hostap_power_save_queue: false,
            }
        );
        source.saved_rsn_ie[2] = 8;
        assert_eq!(destination.saved_rsn_ie, [48, 1, 7]);
    }

    #[test]
    fn station_tx_node_uses_bss_for_unicast_and_multicast_destinations() {
        let table = NodeTable::default();
        assert_eq!(
            find_station_tx_node(&table, true, [2, 0, 0, 0, 0, 4]),
            Some(&table.bss_node)
        );
        assert_eq!(
            find_station_tx_node(&table, false, [1, 0, 0, 0, 0, 4]),
            Some(&table.bss_node)
        );
        assert_eq!(
            find_station_tx_node(&table, false, [2, 0, 0, 0, 0, 4]),
            None
        );
    }

    #[test]
    fn duplicate_rx_node_inherits_bss_address_and_channel() {
        let mut table = NodeTable::default();
        table.bss_node.access_point.bssid = [2, 1, 2, 3, 4, 5];
        table.bss_node.access_point.channel = 36;
        let peer = duplicate_bss_node(&mut table, [2, 9, 8, 7, 6, 5]).unwrap();
        assert_eq!(peer.access_point.bssid, [2, 1, 2, 3, 4, 5]);
        assert_eq!(peer.access_point.channel, 36);
    }
}
