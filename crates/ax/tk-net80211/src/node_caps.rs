//! HT/VHT/HE capability and operation IE parsing from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_node.c` rev 1.217 and
//! `ieee80211_node.h` rev 1.64 (BSD-3-Clause). Copyright (c) 2001 Atsushi
//! Onoe, (c) 2002, 2003 Sam Leffler, Errno Consulting, and (c) 2008 Damien
//! Bergamini.

use crate::{HTOP0_SCO_MASK, HTOP0_SCO_SCN, valid_40mhz_center_frequency};

pub const NODE_HTCAP: u32 = 1 << 0;
pub const NODE_HT: u32 = 1 << 1;
pub const NODE_HT_SGI20: u32 = 1 << 2;
pub const NODE_HT_SGI40: u32 = 1 << 3;
pub const NODE_VHTCAP: u32 = 1 << 4;
pub const NODE_VHT: u32 = 1 << 5;
pub const NODE_VHT_SGI80: u32 = 1 << 6;
pub const NODE_VHT_SGI160: u32 = 1 << 7;
pub const NODE_HECAP: u32 = 1 << 8;
pub const NODE_HE: u32 = 1 << 9;

pub const HE_MAC_CAPS_LEN: usize = 6;
pub const HE_PHY_CAPS_LEN: usize = 11;
pub const HE_FIXED_CAPS_LEN: usize = HE_MAC_CAPS_LEN + HE_PHY_CAPS_LEN;
pub const HE_MCS_NSS_80_LEN: usize = 4;
pub const HE_PHYCAP0_CHAN_WIDTH_160_IN_5G: u8 = 0x08;
pub const HE_PHYCAP0_CHAN_WIDTH_8080_IN_5G: u8 = 0x10;
pub const VHTOP0_CHAN_WIDTH_HT: u8 = 0;
pub const VHTOP0_CHAN_WIDTH_80: u8 = 1;
pub const VHTOP0_CHAN_WIDTH_160: u8 = 2;
pub const VHTOP0_CHAN_WIDTH_8080: u8 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HtCapabilities {
    pub caps: u16,
    pub ampdu_param: u8,
    pub rx_mcs: [u8; 10],
    pub max_rx_rate: u16,
    pub tx_mcs_set: u8,
    pub tx_caps: u16,
    pub tx_beamforming_caps: u32,
    pub antenna_selection_caps: u8,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HtOperation {
    pub primary_channel: u8,
    pub htop0: u8,
    pub htop1: u16,
    pub htop2: u16,
    pub basic_mcs: [u8; 16],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VhtCapabilities {
    pub caps: u32,
    pub rx_mcs: u16,
    pub rx_max_lgi_mbps: u16,
    pub tx_mcs: u16,
    pub tx_max_lgi_mbps: u16,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeCapabilities {
    pub mac_caps: [u8; HE_MAC_CAPS_LEN],
    pub phy_caps: [u8; HE_PHY_CAPS_LEN],
    pub rx_mcs_80: u16,
    pub tx_mcs_80: u16,
    pub rx_mcs_160: u16,
    pub tx_mcs_160: u16,
    pub rx_mcs_80p80: u16,
    pub tx_mcs_80p80: u16,
    pub oper_params: [u8; 4],
    pub basic_mcs: u16,
    pub spatial_streams: u8,
    pub flags: u32,
}

#[inline]
fn le16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

// upstream: ieee80211_node.c ieee80211_setup_htcaps()
pub fn setup_ht_caps(node: &mut HtCapabilities, data: &[u8]) -> bool {
    if data.len() != 26 {
        return false;
    }
    node.caps = le16(data, 0);
    node.ampdu_param = data[2];
    node.rx_mcs.copy_from_slice(&data[3..13]);
    // Upstream clears MCS 77-79 from the 80-bit bitmap after copying.
    node.rx_mcs[9] &= 0x1f;
    let rxrate = le16(data, 13) & 0x03ff;
    if rxrate < 1024 {
        node.max_rx_rate = rxrate;
    }
    node.tx_mcs_set = data[15];
    node.tx_caps = le16(data, 19);
    node.tx_beamforming_caps = u32::from_le_bytes([data[21], data[22], data[23], data[24]]);
    node.antenna_selection_caps = data[25];
    node.flags |= NODE_HTCAP;
    true
}

// upstream: ieee80211_node.c ieee80211_clear_htcaps()
pub fn clear_ht_caps(node: &mut HtCapabilities) {
    node.caps = 0;
    node.ampdu_param = 0;
    node.rx_mcs = [0; 10];
    node.max_rx_rate = 0;
    node.tx_mcs_set = 0;
    node.tx_caps = 0;
    node.tx_beamforming_caps = 0;
    node.antenna_selection_caps = 0;
    node.flags &= !(NODE_HT | NODE_HT_SGI20 | NODE_HT_SGI40 | NODE_HTCAP);
}

// upstream: ieee80211_node.c ieee80211_setup_htop()
pub fn setup_ht_operation(node: &mut HtOperation, data: &[u8], is_probe: bool) -> bool {
    if data.len() != 22 {
        return false;
    }
    node.primary_channel = data[0];
    node.htop0 = data[1];
    if !valid_40mhz_center_frequency(data[0], data[1]) {
        node.htop0 &= !HTOP0_SCO_MASK;
    }
    node.htop1 = le16(data, 2);
    node.htop2 = le16(data, 4);
    if is_probe {
        node.basic_mcs.copy_from_slice(&data[6..22]);
    }
    true
}

// upstream: ieee80211_node.c ieee80211_setup_vhtcaps()
pub fn setup_vht_caps(node: &mut VhtCapabilities, data: &[u8]) -> bool {
    if data.len() != 12 {
        return false;
    }
    node.caps = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    node.rx_mcs = le16(data, 4);
    node.rx_max_lgi_mbps = le16(data, 6) & 0x1fff;
    node.tx_mcs = le16(data, 8);
    node.tx_max_lgi_mbps = le16(data, 10);
    node.flags |= NODE_VHTCAP;
    true
}

// upstream: ieee80211_node.c ieee80211_clear_vhtcaps()
pub fn clear_vht_caps(node: &mut VhtCapabilities) {
    node.caps = 0;
    node.rx_mcs = 0;
    node.rx_max_lgi_mbps = 0;
    node.tx_mcs = 0;
    node.tx_max_lgi_mbps = 0;
    node.flags &= !(NODE_VHT | NODE_VHT_SGI80 | NODE_VHT_SGI160 | NODE_VHTCAP);
}

// upstream: ieee80211_node.c ieee80211_setup_hecaps()
pub fn setup_he_caps(node: &mut HeCapabilities, data: &[u8]) -> bool {
    if data.len() < HE_FIXED_CAPS_LEN + HE_MCS_NSS_80_LEN {
        return false;
    }
    let phycap0 = data[HE_MAC_CAPS_LEN];
    let mcs_len = HE_MCS_NSS_80_LEN
        + if phycap0 & HE_PHYCAP0_CHAN_WIDTH_160_IN_5G != 0 {
            4
        } else {
            0
        }
        + if phycap0 & HE_PHYCAP0_CHAN_WIDTH_8080_IN_5G != 0 {
            4
        } else {
            0
        };
    if data.len() < HE_FIXED_CAPS_LEN + mcs_len {
        return false;
    }
    node.mac_caps.copy_from_slice(&data[..HE_MAC_CAPS_LEN]);
    node.phy_caps
        .copy_from_slice(&data[HE_MAC_CAPS_LEN..HE_FIXED_CAPS_LEN]);
    let mut pos = HE_FIXED_CAPS_LEN;
    node.rx_mcs_80 = le16(data, pos);
    node.tx_mcs_80 = le16(data, pos + 2);
    pos += 4;
    if phycap0 & HE_PHYCAP0_CHAN_WIDTH_160_IN_5G != 0 {
        node.rx_mcs_160 = le16(data, pos);
        node.tx_mcs_160 = le16(data, pos + 2);
        pos += 4;
    } else {
        node.rx_mcs_160 = 0;
        node.tx_mcs_160 = 0;
    }
    if phycap0 & HE_PHYCAP0_CHAN_WIDTH_8080_IN_5G != 0 {
        node.rx_mcs_80p80 = le16(data, pos);
        node.tx_mcs_80p80 = le16(data, pos + 2);
    } else {
        node.rx_mcs_80p80 = 0;
        node.tx_mcs_80p80 = 0;
    }
    node.flags |= NODE_HECAP;
    true
}

// upstream: ieee80211_node.c ieee80211_setup_heop()
pub fn setup_he_operation(node: &mut HeCapabilities, data: &[u8], is_probe: bool) -> bool {
    if data.len() < 6 {
        return false;
    }
    node.oper_params.copy_from_slice(&data[..4]);
    if is_probe {
        node.basic_mcs = le16(data, 4);
    }
    true
}

// upstream: ieee80211_node.c ieee80211_clear_hecaps()
pub fn clear_he_caps(node: &mut HeCapabilities) {
    node.mac_caps = [0; HE_MAC_CAPS_LEN];
    node.phy_caps = [0; HE_PHY_CAPS_LEN];
    node.rx_mcs_80 = 0;
    node.tx_mcs_80 = 0;
    node.rx_mcs_160 = 0;
    node.tx_mcs_160 = 0;
    node.rx_mcs_80p80 = 0;
    node.tx_mcs_80p80 = 0;
    node.oper_params = [0; 4];
    node.basic_mcs = 0;
    node.spatial_streams = 0;
    node.flags &= !(NODE_HE | NODE_HECAP);
}

/// Build firmware's HT secondary offset from accepted HT/channel capabilities.
// upstream: ieee80211_node.c ieee80211_node_ht_secondary_channel_offset()
pub fn ht_secondary_offset(
    node_flags: u32,
    channel_40_allowed: bool,
    peer_supports_40: bool,
    htop0: u8,
) -> u8 {
    if node_flags & NODE_HT != 0 && channel_40_allowed && peer_supports_40 {
        htop0 & HTOP0_SCO_MASK
    } else {
        HTOP0_SCO_SCN
    }
}

/// Return the advertised VHT channel width after channel and peer capability filtering.
// upstream: ieee80211_node.c ieee80211_node_vht_channel_width()
pub fn vht_channel_width(
    node_flags: u32,
    channel_160_allowed: bool,
    peer_supports_160: bool,
    channel_80_allowed: bool,
    peer_supports_80: bool,
) -> u8 {
    if node_flags & NODE_VHT != 0 && channel_160_allowed && peer_supports_160 {
        VHTOP0_CHAN_WIDTH_160
    } else if node_flags & NODE_VHT != 0 && channel_80_allowed && peer_supports_80 {
        VHTOP0_CHAN_WIDTH_80
    } else {
        VHTOP0_CHAN_WIDTH_HT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ht_vht_and_he_capability_layouts_are_length_checked_and_little_endian() {
        let mut ht = HtCapabilities::default();
        let mut ht_data = [0u8; 26];
        ht_data[0] = 0x34;
        ht_data[1] = 0x12;
        ht_data[12] = 0xff;
        ht_data[13] = 0x20;
        ht_data[15] = 3;
        assert!(!setup_ht_caps(&mut ht, &ht_data[..25]));
        assert!(setup_ht_caps(&mut ht, &ht_data));
        assert_eq!(ht.caps, 0x1234);
        assert_eq!(ht.rx_mcs[9] & 0xe0, 0);
        assert_eq!(ht.max_rx_rate, 0x20);
        assert_eq!(ht.flags & NODE_HTCAP, NODE_HTCAP);
        clear_ht_caps(&mut ht);
        assert_eq!(ht.caps, 0);
        assert_eq!(ht.flags & NODE_HTCAP, 0);
        let mut vht = VhtCapabilities::default();
        let mut vht_data = [0u8; 12];
        vht_data[0] = 1;
        vht_data[6] = 0xff;
        vht_data[7] = 0x1f;
        assert!(setup_vht_caps(&mut vht, &vht_data));
        assert_eq!(vht.rx_max_lgi_mbps, 8191);
        clear_vht_caps(&mut vht);
        assert_eq!(vht.flags, 0);
    }
    #[test]
    fn htop_and_he_variable_mcs_lengths_follow_advertised_widths() {
        let mut htop = HtOperation::default();
        let mut data = [0u8; 22];
        data[0] = 36;
        data[1] = 1;
        data[6] = 7;
        assert!(setup_ht_operation(&mut htop, &data, true));
        assert_eq!(htop.basic_mcs[0], 7);
        let mut he = HeCapabilities::default();
        let mut he_data = [0u8; HE_FIXED_CAPS_LEN + 12];
        he_data[HE_MAC_CAPS_LEN] = 0x18;
        he_data[17] = 1;
        he_data[21] = 3;
        he_data[22] = 4;
        he_data[25] = 5;
        he_data[26] = 6;
        assert!(!setup_he_caps(&mut he, &he_data[..HE_FIXED_CAPS_LEN + 11]));
        assert!(setup_he_caps(&mut he, &he_data));
        assert_eq!(he.rx_mcs_80, 1);
        assert_eq!(he.rx_mcs_160, 0x0403);
        assert_eq!(he.rx_mcs_80p80, 0x0605);
        let op = [1, 2, 3, 4, 5, 6];
        assert!(setup_he_operation(&mut he, &op, true));
        assert_eq!(he.basic_mcs, 0x0605);
        clear_he_caps(&mut he);
        assert_eq!(he.rx_mcs_80, 0);
        assert_eq!(he.flags, 0);
    }
    #[test]
    fn channel_width_helpers_gate_on_both_peer_and_local_state() {
        assert_eq!(ht_secondary_offset(NODE_HT, true, true, 1), 1);
        assert_eq!(ht_secondary_offset(0, true, true, 1), 0);
        assert_eq!(
            vht_channel_width(NODE_VHT, true, true, true, true),
            VHTOP0_CHAN_WIDTH_160
        );
        assert_eq!(
            vht_channel_width(NODE_VHT, false, true, true, true),
            VHTOP0_CHAN_WIDTH_80
        );
    }
}
