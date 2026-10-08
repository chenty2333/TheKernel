//! NVM channel-map generation from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{
    NVM_CHANNEL_40MHZ, NVM_CHANNEL_80MHZ, NVM_CHANNEL_160MHZ, NVM_CHANNEL_ACTIVE,
    NVM_CHANNEL_VALID, NvmInfo,
};

pub const CHANNELS_2GHZ: usize = 14;
pub const CHANNELS_5GHZ: usize = 37;
pub const CHANNELS_24_5GHZ: usize = CHANNELS_2GHZ + CHANNELS_5GHZ;

pub const CHAN_CCK: u32 = 1 << 0;
pub const CHAN_OFDM: u32 = 1 << 1;
pub const CHAN_DYN: u32 = 1 << 2;
pub const CHAN_2GHZ: u32 = 1 << 3;
pub const CHAN_A: u32 = 1 << 4;
pub const CHAN_PASSIVE: u32 = 1 << 5;
pub const CHAN_HT: u32 = 1 << 6;
pub const CHAN_40MHZ: u32 = 1 << 7;
pub const CHAN_VHT: u32 = 1 << 8;
pub const CHANX_80MHZ: u32 = 1 << 0;
pub const CHANX_160MHZ: u32 = 1 << 1;
pub const VHT_CTRL_1_BELOW: u8 = 0;
pub const VHT_CTRL_2_BELOW: u8 = 1;
pub const VHT_CTRL_3_BELOW: u8 = 2;
pub const VHT_CTRL_4_BELOW: u8 = 3;
pub const VHT_CTRL_1_ABOVE: u8 = 4;
pub const VHT_CTRL_2_ABOVE: u8 = 5;
pub const VHT_CTRL_3_ABOVE: u8 = 6;
pub const VHT_CTRL_4_ABOVE: u8 = 7;

const CHANNELS_8000: [u8; CHANNELS_24_5GHZ] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 36, 40, 44, 48, 52, 56, 60, 64, 68, 72, 76, 80,
    84, 88, 92, 96, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, 149, 153, 157, 161,
    165, 169, 173, 177, 181,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelInfo {
    pub channel: u8,
    pub frequency_mhz: u16,
    pub flags: u32,
    pub extended_flags: u32,
}

/// Convert the primary/center channel-index delta to firmware's VHT control position.
// upstream: if_iwx.c iwx_get_vht_ctrl_pos()
pub const fn vht_control_position(primary_index: i16, center_index: i16) -> u8 {
    match primary_index - center_index {
        -14 => VHT_CTRL_4_BELOW,
        -10 => VHT_CTRL_3_BELOW,
        -6 => VHT_CTRL_2_BELOW,
        -2 => VHT_CTRL_1_BELOW,
        2 => VHT_CTRL_1_ABOVE,
        6 => VHT_CTRL_2_ABOVE,
        10 => VHT_CTRL_3_ABOVE,
        14 => VHT_CTRL_4_ABOVE,
        _ => VHT_CTRL_1_BELOW,
    }
}

/// Generate the standard 2.4/5-GHz channel table from per-channel NVM flags.
// upstream: if_iwx.c iwx_init_channel_map()
pub fn init_channel_map(nvm: &NvmInfo, uhb_supported: bool) -> Vec<ChannelInfo> {
    let channels: &[u8] = if uhb_supported {
        &CHANNELS_UHB[..CHANNELS_24_5GHZ]
    } else {
        &CHANNELS_8000
    };
    let count = channels.len().min(nvm.channel_profiles.len());
    let mut result = Vec::with_capacity(count);
    for (index, &channel) in channels.iter().take(count).enumerate() {
        if index >= CHANNELS_24_5GHZ {
            break;
        }
        let is_5ghz = index >= CHANNELS_2GHZ;
        let mut profile = nvm.channel_profiles[index];
        if is_5ghz && !nvm.band_52ghz {
            profile &= !NVM_CHANNEL_VALID;
        }
        if profile & NVM_CHANNEL_VALID == 0 {
            result.push(ChannelInfo {
                channel,
                frequency_mhz: 0,
                flags: 0,
                extended_flags: 0,
            });
            continue;
        }
        let mut flags = if is_5ghz {
            CHAN_A
        } else {
            CHAN_CCK | CHAN_OFDM | CHAN_DYN | CHAN_2GHZ
        };
        let mut extended_flags = 0;
        if !is_5ghz {
            if profile & NVM_CHANNEL_ACTIVE == 0 {
                flags |= CHAN_PASSIVE;
            }
        } else {
            if profile & NVM_CHANNEL_ACTIVE == 0 {
                flags |= CHAN_PASSIVE;
            }
        }
        if nvm.supports_11n {
            flags |= CHAN_HT;
            if profile & NVM_CHANNEL_40MHZ != 0 {
                flags |= CHAN_40MHZ;
            }
        }
        if is_5ghz && nvm.supports_11ac {
            flags |= CHAN_VHT;
            if profile & NVM_CHANNEL_80MHZ != 0 {
                extended_flags |= CHANX_80MHZ;
            }
            if profile & NVM_CHANNEL_160MHZ != 0 {
                extended_flags |= CHANX_160MHZ;
            }
        }
        let frequency_mhz = if channel == 14 {
            2484
        } else if is_5ghz {
            5000 + u16::from(channel) * 5
        } else {
            2407 + u16::from(channel) * 5
        };
        result.push(ChannelInfo {
            channel,
            frequency_mhz,
            flags,
            extended_flags,
        });
    }
    result
}

const CHANNELS_UHB: [u8; 110] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 36, 40, 44, 48, 52, 56, 60, 64, 68, 72, 76, 80,
    84, 88, 92, 96, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, 149, 153, 157, 161,
    165, 169, 173, 177, 181, 1, 5, 9, 13, 17, 21, 25, 29, 33, 37, 41, 45, 49, 53, 57, 61, 65, 69,
    73, 77, 81, 85, 89, 93, 97, 101, 105, 109, 113, 117, 121, 125, 129, 133, 137, 141, 145, 149,
    153, 157, 161, 165, 169, 173, 177, 181, 185, 189, 193, 197, 201, 205, 209, 213, 217, 221, 225,
    229, 233,
];

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn nvm() -> NvmInfo {
        NvmInfo {
            hardware_address: [0, 1, 2, 3, 4, 5],
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
            valid_tx_antennas: 3,
            valid_rx_antennas: 3,
            lar_enabled: false,
            channel_profiles: vec![
                NVM_CHANNEL_VALID
                    | NVM_CHANNEL_ACTIVE
                    | NVM_CHANNEL_40MHZ
                    | NVM_CHANNEL_80MHZ
                    | NVM_CHANNEL_160MHZ;
                110
            ],
        }
    }

    #[test]
    fn channel_map_applies_band_profile_and_generation_features() {
        let mut nvm = nvm();
        let channels = init_channel_map(&nvm, true);
        assert_eq!(channels.len(), CHANNELS_24_5GHZ);
        assert_eq!(channels[0].channel, 1);
        assert_eq!(channels[0].frequency_mhz, 2412);
        assert_ne!(channels[0].flags & CHAN_HT, 0);
        assert_eq!(channels[0].extended_flags, 0);
        assert_eq!(channels[13].frequency_mhz, 2484);
        assert_eq!(channels[14].channel, 36);
        assert_eq!(channels[14].frequency_mhz, 5180);
        assert_ne!(channels[14].extended_flags & CHANX_160MHZ, 0);

        nvm.band_52ghz = false;
        let channels = init_channel_map(&nvm, false);
        assert_eq!(channels[14].frequency_mhz, 0);
        assert_eq!(channels[14].flags, 0);
    }

    #[test]
    fn channel_map_truncates_six_ghz_for_net80211_abi() {
        assert_eq!(init_channel_map(&nvm(), true).len(), 51);
    }

    #[test]
    fn vht_control_position_matches_all_supported_160mhz_subchannels() {
        assert_eq!(vht_control_position(36, 50), VHT_CTRL_4_BELOW);
        assert_eq!(vht_control_position(40, 50), VHT_CTRL_3_BELOW);
        assert_eq!(vht_control_position(44, 50), VHT_CTRL_2_BELOW);
        assert_eq!(vht_control_position(48, 50), VHT_CTRL_1_BELOW);
        assert_eq!(vht_control_position(52, 50), VHT_CTRL_1_ABOVE);
        assert_eq!(vht_control_position(56, 50), VHT_CTRL_2_ABOVE);
        assert_eq!(vht_control_position(60, 50), VHT_CTRL_3_ABOVE);
        assert_eq!(vht_control_position(64, 50), VHT_CTRL_4_ABOVE);
        assert_eq!(vht_control_position(42, 50), VHT_CTRL_1_BELOW);
    }
}
