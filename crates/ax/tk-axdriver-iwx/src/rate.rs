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

pub const TLC_CONFIG_GROUP: u8 = 0x05;
pub const TLC_CONFIG_COMMAND: u8 = 0x0f;
pub const TLC_MODE_NON_HT: u8 = 0;
pub const TLC_MODE_HT: u8 = 1;
pub const TLC_MODE_VHT: u8 = 2;
pub const TLC_WIDTH_20: u8 = 0;
pub const TLC_WIDTH_40: u8 = 1;
pub const TLC_WIDTH_80: u8 = 2;
pub const TLC_WIDTH_160: u8 = 3;
pub const TLC_CHAIN_A: u8 = 1;
pub const TLC_CHAIN_B: u8 = 2;
pub const TLC_FLAG_STBC: u16 = 1;
pub const TLC_SGI_20: u8 = 1 << TLC_WIDTH_20;
pub const TLC_SGI_40: u8 = 1 << TLC_WIDTH_40;
pub const TLC_SGI_80: u8 = 1 << TLC_WIDTH_80;
pub const TLC_SGI_160: u8 = 1 << TLC_WIDTH_160;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlRateConfig<'a> {
    pub station_id: u8,
    pub legacy_rates: &'a [u8],
    pub ht: bool,
    pub vht: bool,
    pub peer_ht_rx_mcs: u16,
    pub local_ht_mcs: u16,
    pub peer_vht_rx_mcs: u16,
    pub secondary_channel_offset: u8,
    pub vht_channel_width: u8,
    pub mimo_enabled: bool,
    pub tx_antenna_count: u8,
    pub peer_ht_rx_stbc: bool,
    pub peer_vht_rx_stbc: bool,
    pub ht_sgi20: bool,
    pub ht_sgi40: bool,
    pub vht_sgi80: bool,
    pub vht_sgi160: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlRateConfigError {
    InvalidLegacyRate(u8),
    InvalidVhtMcs(TxRateError),
    UnsupportedVersion(u8),
    Command(crate::CommandError),
}
impl From<crate::CommandError> for TlRateConfigError {
    fn from(error: crate::CommandError) -> Self {
        Self::Command(error)
    }
}

fn tlc_rate_configuration(
    config: TlRateConfig<'_>,
    version: u8,
    slot: u8,
) -> Result<crate::EncodedCommand, TlRateConfigError> {
    if version != 3 && version != 4 {
        return Err(TlRateConfigError::UnsupportedVersion(version));
    }
    let mut bytes = alloc::vec![0u8; if version == 3 { 28 } else { 28 }];
    bytes[0] = config.station_id;
    let mut non_ht_rates = 0u16;
    for rate in config.legacy_rates {
        let idx =
            rateset_11g_index(rate & 0x7f).ok_or(TlRateConfigError::InvalidLegacyRate(*rate))?;
        non_ht_rates |= 1 << idx;
    }
    let mode = if config.vht {
        TLC_MODE_VHT
    } else if config.ht {
        TLC_MODE_HT
    } else {
        TLC_MODE_NON_HT
    };
    bytes[5] = mode;
    let max_width = if config.vht && config.vht_channel_width == 2 {
        TLC_WIDTH_160
    } else if config.vht && config.vht_channel_width == 1 {
        TLC_WIDTH_80
    } else if config.ht && matches!(config.secondary_channel_offset, 1 | 3) {
        TLC_WIDTH_40
    } else {
        TLC_WIDTH_20
    };
    bytes[4] = max_width;
    bytes[6] = if config.ht && config.mimo_enabled {
        TLC_CHAIN_A | TLC_CHAIN_B
    } else {
        TLC_CHAIN_A
    };
    if version == 3 {
        bytes[8..10].copy_from_slice(
            &(if config.tx_antenna_count > 1
                && ((config.vht && config.peer_vht_rx_stbc)
                    || (config.ht && config.peer_ht_rx_stbc))
            {
                TLC_FLAG_STBC
            } else {
                0
            })
            .to_le_bytes(),
        );
        bytes[10..12].copy_from_slice(&non_ht_rates.to_le_bytes());
    } else {
        bytes[7] = sgi_bitmap(config);
        bytes[8..10].copy_from_slice(
            &(if config.tx_antenna_count > 1
                && ((config.vht && config.peer_vht_rx_stbc)
                    || (config.ht && config.peer_ht_rx_stbc))
            {
                TLC_FLAG_STBC
            } else {
                0
            })
            .to_le_bytes(),
        );
        bytes[10..12].copy_from_slice(&non_ht_rates.to_le_bytes());
    }
    if config.vht {
        for nss in 1..=if config.mimo_enabled { 2 } else { 1 } {
            let bitmap = rateset_vht_bitmap(
                config.peer_vht_rx_mcs,
                nss,
                config.secondary_channel_offset == 1 || config.secondary_channel_offset == 3,
            )
            .map_err(TlRateConfigError::InvalidVhtMcs)?;
            let nss_slot = usize::from(nss - 1);
            put_tlc_u16(
                &mut bytes,
                12 + (nss_slot * if version == 3 { 2 } else { 3 }) * 2,
                bitmap,
            );
            if version == 3 && config.vht_channel_width == 2 {
                put_tlc_u16(&mut bytes, 12 + (nss_slot * 2 + 1) * 2, bitmap);
            } else if version == 4 && config.vht_channel_width == 2 {
                put_tlc_u16(&mut bytes, 12 + (nss_slot * 3 + 1) * 2, bitmap);
            }
        }
    } else if config.ht {
        put_tlc_u16(
            &mut bytes,
            12,
            rateset_ht_bitmap(config.peer_ht_rx_mcs, config.local_ht_mcs, HtRateSet::Siso),
        );
        if config.mimo_enabled {
            put_tlc_u16(
                &mut bytes,
                16,
                rateset_ht_bitmap(config.peer_ht_rx_mcs, config.local_ht_mcs, HtRateSet::Mimo2),
            );
        }
    }
    let max_mpdu = if config.vht {
        3895u16
    } else if config.ht {
        3839
    } else {
        2316
    };
    if version == 3 {
        bytes[20..22].copy_from_slice(&max_mpdu.to_le_bytes());
        bytes[22] = sgi_bitmap(config);
    } else {
        bytes[24..26].copy_from_slice(&max_mpdu.to_le_bytes());
    }
    let command = crate::HostCommand {
        id: (u32::from(TLC_CONFIG_GROUP) << 8) | u32::from(TLC_CONFIG_COMMAND),
        flags: crate::CMD_ASYNC,
        response_capacity: 0,
        parts: &[&bytes],
    };
    Ok(crate::EncodedCommand::encode(&command, slot, 0)?)
}

/// Build the source API-v3 TLC rate configuration command.
// upstream: if_iwx.c iwx_rs_init_v3()
pub fn tlc_rate_command_v3(
    config: TlRateConfig<'_>,
    slot: u8,
) -> Result<crate::EncodedCommand, TlRateConfigError> {
    tlc_rate_configuration(config, 3, slot)
}

/// Build the source API-v4 TLC rate configuration command.
// upstream: if_iwx.c iwx_rs_init_v4()
pub fn tlc_rate_command_v4(
    config: TlRateConfig<'_>,
    slot: u8,
) -> Result<crate::EncodedCommand, TlRateConfigError> {
    tlc_rate_configuration(config, 4, slot)
}

/// Select TLC configuration layout version 4 when explicitly advertised, else v3.
// upstream: if_iwx.c iwx_rs_init()
pub fn init_rate_command(
    config: TlRateConfig<'_>,
    command_version: u8,
    slot: u8,
) -> Result<crate::EncodedCommand, TlRateConfigError> {
    if command_version == 4 {
        tlc_rate_command_v4(config, slot)
    } else {
        tlc_rate_command_v3(config, slot)
    }
}

pub const TLC_RATE_UPDATE_FLAG: u32 = 1;
pub const TLC_UPDATE_NOTIFICATION: u8 = 0xf7;
pub const TLC_RATE_STATION_ID: u8 = 0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerTxRateState {
    pub tx_rate_index: u8,
    pub tx_mcs: u8,
    pub vht_spatial_streams: u8,
}

/// Apply a TLC initial-rate notification to the associated peer's TX-rate state.
// upstream: if_iwx.c iwx_rs_update()
pub fn apply_tlc_rate_update(
    current: &mut PeerTxRateState,
    peer_rates: &[u8],
    station_id: u8,
    flags: u32,
    rate_n_flags: u32,
    notification_version: u8,
) -> bool {
    if station_id != TLC_RATE_STATION_ID || flags & TLC_RATE_UPDATE_FLAG == 0 {
        return false;
    }
    if notification_version != 99 && notification_version >= 3 {
        match rate_n_flags & (0x7 << 8) {
            0x300 => {
                current.tx_mcs = (rate_n_flags & 0xf) as u8;
                current.vht_spatial_streams = (((rate_n_flags >> 4) & 1) + 1) as u8;
                return true;
            }
            0x200 => {
                current.tx_mcs = (((rate_n_flags & (1 << 4)) >> 1) | (rate_n_flags & 0x7)) as u8;
                return true;
            }
            _ => {}
        }
    } else {
        if rate_n_flags & (1 << 26) != 0 {
            current.tx_mcs = (rate_n_flags & 0xf) as u8;
            current.vht_spatial_streams = (((rate_n_flags >> 4) & 0x3) + 1) as u8;
            return true;
        }
        if rate_n_flags & (1 << 8) != 0 {
            current.tx_mcs = (rate_n_flags & 0x1f) as u8;
            return true;
        }
    }
    let rate_value = if notification_version != 99 && notification_version >= 3 {
        let rate_index = (rate_n_flags & 0x7) as usize;
        let rates: &[u8] = if rate_n_flags & (1 << 8) != 0 {
            &[12, 18, 24, 36, 48, 72, 96, 108]
        } else {
            &[2, 4, 11, 22]
        };
        rates.get(rate_index).copied().unwrap_or(0)
    } else {
        let plcp = (rate_n_flags & 0xff) as u8;
        RATES
            .iter()
            .find(|rate| rate.legacy_plcp == Some(plcp))
            .map_or(0, |rate| rate.value as u8)
    };
    if rate_value != 0 {
        if let Some(index) = peer_rates.iter().position(|peer| peer & 0x7f == rate_value) {
            current.tx_rate_index = index as u8;
            return true;
        }
    }
    false
}

fn sgi_bitmap(config: TlRateConfig<'_>) -> u8 {
    (if config.ht && config.ht_sgi20 {
        TLC_SGI_20
    } else {
        0
    }) | (if config.ht && config.ht_sgi40 {
        TLC_SGI_40
    } else {
        0
    }) | (if config.vht && config.vht_sgi80 {
        TLC_SGI_80
    } else {
        0
    }) | (if config.vht && config.vht_channel_width == 2 && config.vht_sgi160 {
        TLC_SGI_160
    } else {
        0
    })
}

fn put_tlc_u16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

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
    let Some(max_mcs) = tk_net80211::get_vht_max_mcs(peer_mcs_map, spatial_stream, supports_40mhz)
    else {
        return Ok(0);
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

const FW_PHY_TX_CHAIN_SHIFT: u32 = 16;
const FW_PHY_RX_CHAIN_SHIFT: u32 = 20;
const ANT_A: u8 = 1 << 0;
const ANT_B: u8 = 1 << 1;
const ANT_C: u8 = 1 << 2;
const ANT_AB: u8 = ANT_A | ANT_B;
const ANT_BC: u8 = ANT_B | ANT_C;
const VHT_MCS_0_9: u16 = 2;
const VHT_MCS_NOT_SUPPORTED: u16 = 3;

/// Intersect the firmware transmit-chain mask with NVM's valid chains.
// upstream: if_iwx.c iwx_fw_valid_tx_ant()
pub const fn valid_tx_antenna_mask(firmware_phy_config: u32, nvm: &crate::NvmInfo) -> u8 {
    let mut tx = ((firmware_phy_config >> FW_PHY_TX_CHAIN_SHIFT) & 0xf) as u8;
    if nvm.valid_tx_antennas != 0 {
        tx &= nvm.valid_tx_antennas;
    }
    tx
}

/// Intersect the firmware receive-chain mask with NVM's valid chains.
// upstream: if_iwx.c iwx_fw_valid_rx_ant()
pub const fn valid_rx_antenna_mask(firmware_phy_config: u32, nvm: &crate::NvmInfo) -> u8 {
    let mut rx = ((firmware_phy_config >> FW_PHY_RX_CHAIN_SHIFT) & 0xf) as u8;
    if nvm.valid_rx_antennas != 0 {
        rx &= nvm.valid_rx_antennas;
    }
    rx
}

/// Collect both firmware/NVM chain masks for a single caller.
pub const fn valid_antenna_masks(firmware_phy_config: u32, nvm: &crate::NvmInfo) -> (u8, u8) {
    let tx = valid_tx_antenna_mask(firmware_phy_config, nvm);
    let rx = valid_rx_antenna_mask(firmware_phy_config, nvm);
    (tx, rx)
}

/// Count only the three physical antenna-chain bits known to OpenBSD iwx.
// upstream: if_iwx.c iwx_fw_num_ant()
pub const fn antenna_count(mask: u8) -> u8 {
    ((mask & ANT_A != 0) as u8) + ((mask & ANT_B != 0) as u8) + ((mask & ANT_C != 0) as u8)
}

/// MIMO is disabled by either the NVM SKU or the user's NO MIMO option.
// upstream: if_iwx.c iwx_mimo_enabled()
pub const fn mimo_enabled(nvm: &crate::NvmInfo, user_no_mimo: bool) -> bool {
    !nvm.mimo_disabled && !user_no_mimo
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HtRateCapabilities {
    pub tx_mcs_set_defined: bool,
    pub supported_mcs: [u8; 16],
    pub tx_stbc: bool,
    pub rx_stbc_streams: u8,
}

/// Build the HT MCS bitmap and STBC caps from the selected antenna masks.
// upstream: if_iwx.c iwx_setup_ht_rates()
pub fn setup_ht_rate_capabilities(tx_ant: u8, rx_ant: u8, mimo: bool) -> HtRateCapabilities {
    let mut supported_mcs = [0; 16];
    supported_mcs[0] = 0xff;
    if mimo && (rx_ant & ANT_AB == ANT_AB || rx_ant & ANT_BC == ANT_BC) {
        supported_mcs[1] = 0xff;
    }
    HtRateCapabilities {
        tx_mcs_set_defined: true,
        supported_mcs,
        tx_stbc: antenna_count(tx_ant) > 1,
        rx_stbc_streams: 1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VhtRateCapabilities {
    pub rx_mcs_map: u16,
    pub tx_mcs_map: u16,
    pub rx_stbc_streams: u8,
    pub rx_antenna_pattern: bool,
    pub tx_stbc: bool,
    pub tx_antenna_pattern: bool,
}

/// Build the per-stream VHT MCS map and antenna-pattern/STBC policy.
// upstream: if_iwx.c iwx_setup_vht_rates()
pub fn setup_vht_rate_capabilities(tx_ant: u8, rx_ant: u8, mimo: bool) -> VhtRateCapabilities {
    let mut rx_count = antenna_count(rx_ant);
    let mut tx_count = antenna_count(tx_ant);
    if !mimo {
        rx_count = 1;
        tx_count = 1;
    }
    let mut mcs_map = VHT_MCS_0_9;
    if mimo && (rx_ant & ANT_AB == ANT_AB || rx_ant & ANT_BC == ANT_BC) {
        mcs_map |= VHT_MCS_0_9 << 2;
    } else {
        mcs_map |= VHT_MCS_NOT_SUPPORTED << 2;
    }
    for stream in 3..=8 {
        mcs_map |= VHT_MCS_NOT_SUPPORTED << (2 * (stream - 1));
    }
    VhtRateCapabilities {
        rx_mcs_map: mcs_map,
        tx_mcs_map: mcs_map,
        rx_stbc_streams: 1,
        rx_antenna_pattern: rx_count == 1,
        tx_stbc: tx_count > 1,
        tx_antenna_pattern: tx_count <= 1,
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

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
    fn tlc_v3_v4_commands_encode_width_mcs_and_rate_update_version() {
        let config = TlRateConfig {
            station_id: 0,
            legacy_rates: &[2, 12, 24],
            ht: true,
            vht: true,
            peer_ht_rx_mcs: 0x01ff,
            local_ht_mcs: 0x00ff,
            peer_vht_rx_mcs: 0xfffa,
            secondary_channel_offset: 1,
            vht_channel_width: 2,
            mimo_enabled: true,
            tx_antenna_count: 2,
            peer_ht_rx_stbc: true,
            peer_vht_rx_stbc: false,
            ht_sgi20: true,
            ht_sgi40: true,
            vht_sgi80: true,
            vht_sgi160: true,
        };
        let v3 = tlc_rate_command_v3(config, 1).unwrap();
        assert_eq!(v3.flags, crate::CMD_ASYNC);
        assert_eq!(v3.bytes.len(), 8 + 28);
        let p3 = &v3.bytes[8..];
        assert_eq!(p3[4], TLC_WIDTH_160);
        assert_eq!(p3[5], TLC_MODE_VHT);
        assert_eq!(
            u16::from_le_bytes(p3[8..10].try_into().unwrap()),
            TLC_FLAG_STBC
        );
        assert_eq!(
            u16::from_le_bytes(p3[10..12].try_into().unwrap()),
            (1 << 0) | (1 << 4) | (1 << 6)
        );
        assert_eq!(u16::from_le_bytes(p3[12..14].try_into().unwrap()), 0x03ff);
        assert_eq!(u16::from_le_bytes(p3[14..16].try_into().unwrap()), 0x03ff);
        assert_eq!(u16::from_le_bytes(p3[16..18].try_into().unwrap()), 0x03ff);
        assert_eq!(u16::from_le_bytes(p3[20..22].try_into().unwrap()), 3895);
        assert_eq!(p3[22], TLC_SGI_20 | TLC_SGI_40 | TLC_SGI_80 | TLC_SGI_160);

        let v4 = tlc_rate_command_v4(config, 1).unwrap();
        assert_eq!(v4.bytes.len(), 8 + 28);
        let p4 = &v4.bytes[8..];
        assert_eq!(p4[7], TLC_SGI_20 | TLC_SGI_40 | TLC_SGI_80 | TLC_SGI_160);
        assert_eq!(u16::from_le_bytes(p4[18..20].try_into().unwrap()), 0x03ff);
        assert_eq!(u16::from_le_bytes(p4[24..26].try_into().unwrap()), 3895);
        assert_eq!(init_rate_command(config, 99, 1).unwrap().bytes, v3.bytes);
    }

    #[test]
    fn tlc_notifications_decode_versioned_mcs_and_legacy_rate_fields() {
        let mut peer = PeerTxRateState::default();
        assert!(!apply_tlc_rate_update(&mut peer, &[12], 1, 1, 0x123, 3));
        assert!(!apply_tlc_rate_update(&mut peer, &[12], 0, 0, 0x123, 3));
        assert!(apply_tlc_rate_update(
            &mut peer,
            &[12],
            0,
            1,
            (3 << 8) | (1 << 4) | 9,
            3
        ));
        assert_eq!(peer.tx_mcs, 9);
        assert_eq!(peer.vht_spatial_streams, 2);
        assert!(apply_tlc_rate_update(
            &mut peer,
            &[2, 12, 24],
            0,
            1,
            (2 << 8) | 0x13,
            3
        ));
        assert_eq!(peer.tx_mcs, 11);
        assert!(apply_tlc_rate_update(
            &mut peer,
            &[2, 4, 12, 24],
            0,
            1,
            (1 << 8) | 2,
            3
        ));
        assert_eq!(peer.tx_rate_index, 3);
        assert!(apply_tlc_rate_update(&mut peer, &[2, 4, 12], 0, 1, 13, 99));
        assert_eq!(peer.tx_rate_index, 2);
        assert!(apply_tlc_rate_update(
            &mut peer,
            &[2, 4, 12],
            0,
            1,
            (1 << 26) | (1 << 4) | 7,
            2
        ));
        assert_eq!(peer.tx_mcs, 7);
        assert_eq!(peer.vht_spatial_streams, 2);
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

    #[test]
    fn antenna_and_ht_vht_caps_follow_firmware_nvm_and_mimo_policy() {
        let nvm = crate::NvmInfo {
            hardware_address: [2, 1, 2, 3, 4, 5],
            nvm_version: 1,
            board_type: 0,
            hardware_address_count: 1,
            empty_otp: false,
            band_24ghz: true,
            band_52ghz: true,
            supports_11n: true,
            supports_11ac: true,
            supports_11ax: true,
            mimo_disabled: false,
            valid_tx_antennas: 0x3,
            valid_rx_antennas: 0x6,
            lar_enabled: false,
            channel_profiles: Vec::new(),
        };
        let firmware_phy = (0x7 << FW_PHY_TX_CHAIN_SHIFT) | (0x7 << FW_PHY_RX_CHAIN_SHIFT);
        let (tx, rx) = valid_antenna_masks(firmware_phy, &nvm);
        assert_eq!((tx, rx), (0x3, 0x6));
        assert_eq!(antenna_count(0x7), 3);
        assert!(mimo_enabled(&nvm, false));

        let ht = setup_ht_rate_capabilities(tx, rx, true);
        assert!(ht.tx_mcs_set_defined && ht.tx_stbc);
        assert_eq!(ht.supported_mcs[0], 0xff);
        assert_eq!(ht.supported_mcs[1], 0xff); // B+C receive pair
        assert_eq!(ht.rx_stbc_streams, 1);

        let vht = setup_vht_rate_capabilities(tx, rx, true);
        assert_eq!(vht.rx_mcs_map & 0x03, VHT_MCS_0_9);
        assert_eq!((vht.rx_mcs_map >> 2) & 0x03, VHT_MCS_0_9);
        assert!(vht.tx_stbc);
        assert!(!vht.tx_antenna_pattern);

        let single = setup_vht_rate_capabilities(1, ANT_A, false);
        assert_eq!((single.rx_mcs_map >> 2) & 0x03, VHT_MCS_NOT_SUPPORTED);
        assert!(single.rx_antenna_pattern && single.tx_antenna_pattern);
        assert!(!mimo_enabled(&nvm, true));
    }
}
