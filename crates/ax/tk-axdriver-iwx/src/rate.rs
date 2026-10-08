//! Rate-index and management-frame TX-rate selection from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub value: u16,
    pub legacy_plcp: Option<u8>,
    pub ht_plcp: Option<u8>,
}

/// The exact `iwx_rates[]` order used for the firmware's rate indices.
pub const RATES: [Rate; 21] = [
    Rate {
        value: 2,
        legacy_plcp: Some(10),
        ht_plcp: None,
    },
    Rate {
        value: 4,
        legacy_plcp: Some(20),
        ht_plcp: None,
    },
    Rate {
        value: 11,
        legacy_plcp: Some(55),
        ht_plcp: None,
    },
    Rate {
        value: 22,
        legacy_plcp: Some(110),
        ht_plcp: None,
    },
    Rate {
        value: 12,
        legacy_plcp: Some(13),
        ht_plcp: Some(0),
    },
    Rate {
        value: 18,
        legacy_plcp: Some(15),
        ht_plcp: None,
    },
    Rate {
        value: 24,
        legacy_plcp: Some(5),
        ht_plcp: Some(1),
    },
    Rate {
        value: 26,
        legacy_plcp: None,
        ht_plcp: Some(8),
    },
    Rate {
        value: 36,
        legacy_plcp: Some(7),
        ht_plcp: Some(2),
    },
    Rate {
        value: 48,
        legacy_plcp: Some(9),
        ht_plcp: Some(3),
    },
    Rate {
        value: 52,
        legacy_plcp: None,
        ht_plcp: Some(9),
    },
    Rate {
        value: 72,
        legacy_plcp: Some(11),
        ht_plcp: Some(4),
    },
    Rate {
        value: 78,
        legacy_plcp: None,
        ht_plcp: Some(10),
    },
    Rate {
        value: 96,
        legacy_plcp: Some(1),
        ht_plcp: Some(5),
    },
    Rate {
        value: 104,
        legacy_plcp: None,
        ht_plcp: Some(11),
    },
    Rate {
        value: 108,
        legacy_plcp: Some(3),
        ht_plcp: Some(6),
    },
    Rate {
        value: 128,
        legacy_plcp: None,
        ht_plcp: Some(7),
    },
    Rate {
        value: 156,
        legacy_plcp: None,
        ht_plcp: Some(12),
    },
    Rate {
        value: 208,
        legacy_plcp: None,
        ht_plcp: Some(13),
    },
    Rate {
        value: 234,
        legacy_plcp: None,
        ht_plcp: Some(14),
    },
    Rate {
        value: 260,
        legacy_plcp: None,
        ht_plcp: Some(15),
    },
];

pub const MCS_TO_RATE_INDEX: [usize; 16] =
    [4, 6, 8, 9, 11, 13, 15, 16, 7, 10, 12, 14, 17, 18, 19, 20];
pub const TX_FLAG_COMMAND_RATE: u16 = 1 << 0;
pub const TX_FLAG_HIGH_PRIORITY: u16 = 1 << 2;

/// Convert a legacy 11a rate value (in 500-kbit/s units) to firmware index.
// upstream: if_iwx.c iwx_fw_rateidx_ofdm()
pub fn fw_rate_index_ofdm(rate: u8) -> u32 {
    [12u8, 18, 24, 36, 48, 72, 96, 108]
        .iter()
        .position(|candidate| *candidate == rate)
        .unwrap_or(0) as u32
}

/// Convert a legacy 11b rate value (in 500-kbit/s units) to firmware index.
// upstream: if_iwx.c iwx_fw_rateidx_cck()
pub fn fw_rate_index_cck(rate: u8) -> u32 {
    [2u8, 4, 11, 22]
        .iter()
        .position(|candidate| *candidate == rate)
        .unwrap_or(0) as u32
}

/// Return the table index, skipping HT-only rows without a legacy PLCP.
// upstream: if_iwx.c iwx_rval2ridx()
pub fn rate_value_to_index(rate: u16) -> usize {
    RATES
        .iter()
        .position(|candidate| candidate.legacy_plcp.is_some() && candidate.value == rate)
        .unwrap_or(RATES.len())
}

/// Find the peer's original rate-set byte for one iwx rate-table index.
// upstream: if_iwx.c iwx_ridx2rate()
pub fn rate_index_to_peer_rate(peer_rates: &[u8], rate_index: usize) -> u8 {
    let Some(rate) = RATES.get(rate_index) else {
        return 0;
    };
    peer_rates
        .iter()
        .copied()
        .find(|value| u16::from(*value & 0x7f) == rate.value)
        .unwrap_or(0)
}

/// Convert an 802.11 legacy rate to the exact iwx table row or sentinel.
// upstream: if_iwx.c iwx_rval2ridx()
pub fn legacy_rate_index(rate: u8) -> usize {
    RATES
        .iter()
        .position(|entry| entry.legacy_plcp.is_some() && entry.value == u16::from(rate))
        .unwrap_or(RATES.len())
}

/// Build the firmware CCK/OFDM basic-rate masks, adding lower mandatory rates.
// upstream: if_iwx.c iwx_ack_rates()
pub fn ack_rate_masks(peer_rates: &[u8], two_ghz: bool) -> (u32, u32) {
    const RATE_BASIC: u8 = 0x80;
    let mut cck = 0u32;
    let mut ofdm = 0u32;
    let mut lowest_cck = None;
    let mut lowest_ofdm = None;
    if two_ghz {
        for index in 0..4 {
            if rate_index_to_peer_rate(peer_rates, index) & RATE_BASIC != 0 {
                cck |= 1 << index;
                lowest_cck = Some(lowest_cck.map_or(index, |low: usize| low.min(index)));
            }
        }
    }
    for index in 4..=15 {
        if rate_index_to_peer_rate(peer_rates, index) & RATE_BASIC != 0 {
            ofdm |= 1 << (index - 4);
            lowest_ofdm = Some(lowest_ofdm.map_or(index, |low: usize| low.min(index)));
        }
    }
    if lowest_ofdm.is_some_and(|lowest| 9 < lowest) {
        ofdm |= 1 << (9 - 4);
    }
    if lowest_ofdm.is_some_and(|lowest| 6 < lowest) {
        ofdm |= 1 << (6 - 4);
    }
    ofdm |= 1; // 6 Mbps is mandatory for OFDM.
    if two_ghz {
        for index in (0..4).rev() {
            if lowest_cck.is_some_and(|lowest| index < lowest) {
                cck |= 1 << index;
            }
        }
        cck |= 1; // 1 Mbps is mandatory for DSSS/HR-DSSS.
    }
    (cck, ofdm)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxRateInput<'a> {
    pub multicast: bool,
    pub frame_type: u8,
    pub data_frame_type: u8,
    pub minimum_basic_rate_index: usize,
    pub peer_rates: &'a [u8],
    pub peer_tx_rate: usize,
    pub peer_ht: bool,
    pub peer_tx_mcs: usize,
    pub rsn_enabled: bool,
    pub ptk_negotiating: bool,
    pub rate_n_flags_version: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxRateSelection {
    pub rate_index: usize,
    pub rate_flags: u16,
    pub rate_n_flags: u32,
    pub selected_rate: Rate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxRateError {
    InvalidBasicRate,
    InvalidPeerRate,
    InvalidMcs,
    InvalidSpatialStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HtRateSet {
    Siso,
    Mimo2,
}

/// Map an advertised 11g rate value to the standard 11g rateset index.
// upstream: if_iwx.c iwx_rs_rval2idx()
pub fn rateset_11g_index(rate: u8) -> Option<usize> {
    [2u8, 4, 11, 22, 12, 18, 24, 36, 48, 72, 96, 108]
        .iter()
        .position(|candidate| *candidate == rate)
}

/// Build the RX MCS bitmap supported by both peer and local 11n ratesets.
// upstream: if_iwx.c iwx_rs_ht_rates()
pub fn rateset_ht_bitmap(peer_rx_mcs: u16, local_supported_mcs: u16, set: HtRateSet) -> u16 {
    let (minimum, maximum) = match set {
        HtRateSet::Siso => (0, 7),
        HtRateSet::Mimo2 => (8, 15),
    };
    let mut bitmap = 0u16;
    for mcs in minimum..=maximum {
        if peer_rx_mcs & (1 << mcs) == 0 || local_supported_mcs & (1 << mcs) == 0 {
            continue;
        }
        bitmap |= 1 << (mcs - minimum);
    }
    bitmap
}

/// Build the VHT receive-MCS bitmap for one spatial stream.
// upstream: if_iwx.c iwx_rs_vht_rates()
pub fn rateset_vht_bitmap(
    peer_mcs_map: u16,
    spatial_stream: u8,
    supports_40mhz: bool,
) -> Result<u16, TxRateError> {
    if !(1..=8).contains(&spatial_stream) {
        return Err(TxRateError::InvalidSpatialStream);
    }
    let shift = u32::from(spatial_stream - 1) * 2;
    let rx_mcs = (peer_mcs_map >> shift) & 0x3;
    let max_mcs = match rx_mcs {
        0 => 7,
        1 => 8,
        2 if supports_40mhz => 9,
        2 => 8,
        3 => return Ok(0),
        _ => return Err(TxRateError::InvalidMcs),
    };
    Ok(((1u16 << (max_mcs + 1)) - 1) & 0x03ff)
}

/// Select firmware rate and flags for management, multicast, or data TX.
// upstream: if_iwx.c iwx_tx_fill_cmd()
pub fn select_tx_rate(input: TxRateInput<'_>) -> Result<TxRateSelection, TxRateError> {
    let minimum = input.minimum_basic_rate_index;
    if minimum >= RATES.len() {
        return Err(TxRateError::InvalidBasicRate);
    }
    let force_rate = input.multicast || input.frame_type != input.data_frame_type;
    let mut flags = 0;
    let index = if force_rate {
        flags |= TX_FLAG_COMMAND_RATE;
        minimum
    } else if input.peer_ht {
        *MCS_TO_RATE_INDEX
            .get(input.peer_tx_mcs)
            .ok_or(TxRateError::InvalidMcs)?
    } else {
        let rate = input
            .peer_rates
            .get(input.peer_tx_rate)
            .ok_or(TxRateError::InvalidPeerRate)?
            & 0x7f;
        let index = rate_value_to_index(u16::from(rate));
        if index == RATES.len() {
            return Err(TxRateError::InvalidPeerRate);
        }
        index.max(minimum)
    };
    if input.rsn_enabled && input.ptk_negotiating {
        flags |= TX_FLAG_HIGH_PRIORITY;
    }
    let rate = RATES[index];
    if !force_rate {
        return Ok(TxRateSelection {
            rate_index: index,
            rate_flags: flags,
            rate_n_flags: 0,
            selected_rate: rate,
        });
    }
    let mut rate_flags = 1 << 14; // IWX_RATE_MCS_ANT_A_MSK
    let is_cck = index < 4;
    if is_cck {
        if input.rate_n_flags_version >= 2 {
            // API v2 modulation type 0 is legacy CCK.
        } else {
            rate_flags |= 1 << 9;
        }
    } else if input.rate_n_flags_version >= 2 {
        rate_flags |= 1 << 8; // legacy OFDM modulation type
    }
    if input.rate_n_flags_version >= 2 {
        let firmware_index = if rate_flags & (1 << 8) != 0 {
            fw_rate_index_ofdm(rate.value as u8)
        } else {
            fw_rate_index_cck(rate.value as u8)
        };
        rate_flags |= (firmware_index as u16) & 0x7;
    } else {
        rate_flags |= u16::from(rate.legacy_plcp.ok_or(TxRateError::InvalidBasicRate)?);
    }
    Ok(TxRateSelection {
        rate_index: index,
        rate_flags: flags,
        rate_n_flags: u32::from(rate_flags),
        selected_rate: rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_rate_order_mcs_map_and_firmware_indices_match() {
        assert_eq!(
            MCS_TO_RATE_INDEX,
            [4, 6, 8, 9, 11, 13, 15, 16, 7, 10, 12, 14, 17, 18, 19, 20]
        );
        assert_eq!(fw_rate_index_ofdm(108), 7);
        assert_eq!(fw_rate_index_ofdm(0), 0);
        assert_eq!(fw_rate_index_cck(22), 3);
        assert_eq!(fw_rate_index_cck(0), 0);
        assert_eq!(rate_value_to_index(26), RATES.len());
    }

    #[test]
    fn ack_masks_add_lower_mandatory_rates_and_keep_basic_bits() {
        let peer_rates = [0x82, 0x8b, 0xb0, 0xec]; // 1/5.5 Mbps CCK; 24/54 Mbps OFDM.
        assert_eq!(rate_index_to_peer_rate(&peer_rates, 0), 0x82);
        assert_eq!(legacy_rate_index(11), 2);
        let (cck, ofdm) = ack_rate_masks(&peer_rates, true);
        assert_eq!(cck, (1 << 0) | (1 << 2));
        assert_eq!(ofdm, (1 << 0) | (1 << 2) | (1 << 5) | (1 << 11));
        assert_eq!(ack_rate_masks(&peer_rates, false).0, 0);
    }

    #[test]
    fn management_rate_forces_versioned_legacy_encoding() {
        let input = TxRateInput {
            multicast: false,
            frame_type: 0,
            data_frame_type: 8,
            minimum_basic_rate_index: 4,
            peer_rates: &[12],
            peer_tx_rate: 0,
            peer_ht: false,
            peer_tx_mcs: 0,
            rsn_enabled: true,
            ptk_negotiating: true,
            rate_n_flags_version: 2,
        };
        let selected = select_tx_rate(input).unwrap();
        assert_eq!(selected.rate_index, 4);
        assert_eq!(
            selected.rate_flags,
            TX_FLAG_COMMAND_RATE | TX_FLAG_HIGH_PRIORITY
        );
        assert_eq!(selected.rate_n_flags, (1 << 14) | (1 << 8));
    }

    #[test]
    fn data_frames_defer_rate_choice_to_firmware_and_respect_minimum_basic_rate() {
        let input = TxRateInput {
            multicast: false,
            frame_type: 8,
            data_frame_type: 8,
            minimum_basic_rate_index: 4,
            peer_rates: &[2],
            peer_tx_rate: 0,
            peer_ht: false,
            peer_tx_mcs: 0,
            rsn_enabled: false,
            ptk_negotiating: false,
            rate_n_flags_version: 1,
        };
        let selected = select_tx_rate(input).unwrap();
        assert_eq!(selected.rate_index, 4);
        assert_eq!(selected.rate_n_flags, 0);
    }

    #[test]
    fn tx_rate_adaptation_maps_11g_ht_and_vht_receive_capabilities() {
        assert_eq!(rateset_11g_index(12), Some(4));
        assert_eq!(rateset_11g_index(0), None);
        assert_eq!(rateset_ht_bitmap(0xffff, 0x00f5, HtRateSet::Siso), 0xf5);
        assert_eq!(rateset_ht_bitmap(0xffff, 0x8500, HtRateSet::Mimo2), 0x85);
        assert_eq!(rateset_vht_bitmap(0b10, 1, true), Ok(0x03ff));
        assert_eq!(rateset_vht_bitmap(0b10, 1, false), Ok(0x01ff));
        assert_eq!(rateset_vht_bitmap(0b01, 1, true), Ok(0x01ff));
        assert_eq!(rateset_vht_bitmap(0b11, 1, true), Ok(0));
        assert_eq!(
            rateset_vht_bitmap(0, 0, true),
            Err(TxRateError::InvalidSpatialStream)
        );
    }
}
