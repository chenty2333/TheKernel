//! IEEE channel/frequency conversions from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211.c` rev 1.92 (BSD-3-Clause).
//! Copyright (c) 2001 Atsushi Onoe. Copyright (c) 2002, 2003 Sam Leffler,
//! Errno Consulting.

use alloc::{vec, vec::Vec};

use crate::PhyMode;

pub const CHAN_2GHZ: u32 = 0x0080;
pub const CHAN_5GHZ: u32 = 0x0100;
pub const NET_CHAN_CCK: u16 = 0x0020;
pub const NET_CHAN_OFDM: u16 = 0x0040;
pub const NET_CHAN_2GHZ: u16 = 0x0080;
pub const NET_CHAN_5GHZ: u16 = 0x0100;
pub const NET_CHAN_PASSIVE: u16 = 0x0200;
pub const NET_CHAN_DYN: u16 = 0x0400;
pub const NET_CHAN_HT: u16 = 0x2000;
pub const NET_CHAN_VHT: u16 = 0x4000;
pub const NET_CHAN_A: u16 = NET_CHAN_5GHZ | NET_CHAN_OFDM;
pub const NET_CHAN_B: u16 = NET_CHAN_2GHZ | NET_CHAN_CCK;
pub const NET_CHAN_PURE_G: u16 = NET_CHAN_2GHZ | NET_CHAN_OFDM;
pub const NET_CHAN_40MHZ: u16 = 0x8000;
pub const NET_CHAN_X_80MHZ: u32 = 0x01;
pub const NET_CHAN_X_160MHZ: u32 = 0x02;
pub const NET_CHAN_X_HE: u32 = 0x04;
pub const NET_CAP_TX_AMPDU: u32 = 1 << 0;
pub const NET_CAP_QOS: u32 = 1 << 1;
pub const NET_FLAG_QOS: u32 = 1 << 0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetChannel {
    pub frequency_mhz: u16,
    pub flags: u16,
    pub extended_flags: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeSelection {
    pub mode: PhyMode,
    pub active: Vec<bool>,
    pub ibss_channel: usize,
}

/// Fill net80211's available-channel set and mode capabilities.
// upstream: ieee80211.c ieee80211_channel_init()
pub fn initialize_channels(
    channels: &mut [NetChannel],
    modecaps: &mut u32,
    current_mode: &mut PhyMode,
) -> Vec<bool> {
    let mut available = vec![false; channels.len()];
    *modecaps |= 1 << PhyMode::Auto as u8;
    for (index, channel) in channels.iter_mut().enumerate() {
        if channel.flags == 0 {
            continue;
        }
        available[index] = true;
        if channel.flags & NET_CHAN_A == NET_CHAN_A {
            *modecaps |= 1 << PhyMode::A as u8;
        }
        if channel.flags & NET_CHAN_B == NET_CHAN_B {
            *modecaps |= 1 << PhyMode::B as u8;
        }
        if channel.flags & NET_CHAN_PURE_G == NET_CHAN_PURE_G {
            *modecaps |= 1 << PhyMode::G as u8;
        }
        if channel.flags & NET_CHAN_HT != 0 {
            *modecaps |= 1 << PhyMode::N as u8;
        }
        if channel.flags & NET_CHAN_VHT != 0 {
            *modecaps |= 1 << PhyMode::Ac as u8;
        }
        if channel.extended_flags & NET_CHAN_X_HE != 0 {
            *modecaps |= 1 << PhyMode::Ax as u8;
        }
    }
    if *modecaps & (1 << *current_mode as u8) == 0 {
        *current_mode = PhyMode::Auto;
    }
    available
}

/// Select a PHY mode and active channels, preserving the source fallback.
// upstream: ieee80211.c ieee80211_setmode()
pub fn select_mode(
    channels: &[NetChannel],
    modecaps: u32,
    mode: PhyMode,
    current_ibss_channel: Option<usize>,
) -> Result<ModeSelection, ()> {
    if modecaps & (1 << mode as u8) == 0 {
        return Err(());
    }
    let modeflags = match mode {
        PhyMode::Auto | PhyMode::Ax => 0,
        PhyMode::A => NET_CHAN_A,
        PhyMode::B => NET_CHAN_B,
        PhyMode::G => NET_CHAN_PURE_G,
        PhyMode::N => NET_CHAN_HT,
        PhyMode::Ac => NET_CHAN_VHT,
    };
    let modexflags = if mode == PhyMode::Ax {
        NET_CHAN_X_HE
    } else {
        0
    };
    let found = channels.iter().any(|channel| {
        if mode == PhyMode::Auto {
            channel.flags != 0
        } else {
            channel.flags != 0
                && channel.flags & modeflags == modeflags
                && channel.extended_flags & modexflags == modexflags
        }
    });
    if !found {
        return Err(());
    }
    let active: Vec<bool> = channels
        .iter()
        .map(|channel| {
            if mode == PhyMode::Auto {
                channel.flags != 0
            } else {
                channel.flags & modeflags == modeflags
            }
        })
        .collect();
    let ibss_channel = current_ibss_channel
        .filter(|&index| active.get(index).copied().unwrap_or(false))
        .or_else(|| active.iter().position(|&is_active| is_active))
        .ok_or(())?;
    Ok(ModeSelection {
        mode,
        active,
        ibss_channel,
    })
}

/// Set or clear the QoS flag only when TX AMPDU and QoS are both supported.
// upstream: ieee80211.c ieee80211_configure_ampdu_tx()
pub fn configure_ampdu_tx(capabilities: u32, enable: bool, flags: &mut u32) {
    if capabilities & (NET_CAP_TX_AMPDU | NET_CAP_QOS) != NET_CAP_TX_AMPDU | NET_CAP_QOS {
        return;
    }
    if enable {
        *flags |= NET_FLAG_QOS;
    } else {
        *flags &= !NET_FLAG_QOS;
    }
}

/// Find a rate by its value, ignoring the IEEE basic-rate bit.
// upstream: ieee80211.c ieee80211_findrate()
pub fn find_rate(rates: &crate::RateSet, rate: u8) -> Option<usize> {
    rates.rates[..rates.count.min(crate::RATE_MAX_SIZE)]
        .iter()
        .position(|candidate| candidate & crate::LEGACY_RATE_VALUE == rate)
}

/// Choose the next PHY mode for a background scan, skipping superset modes.
// upstream: ieee80211.c ieee80211_next_mode()
pub fn next_scan_mode(
    current: PhyMode,
    modecaps: u32,
    fixed_media_mode: bool,
    scan_all_bands: bool,
) -> PhyMode {
    if fixed_media_mode {
        return PhyMode::Auto;
    }
    if scan_all_bands {
        return PhyMode::Auto;
    }
    let mut next = current as u8 + 1;
    while next <= 7 {
        // 11n, 11ac and 11ax channels are supersets of the legacy mode sets.
        if matches!(next, 4 | 5 | 6) {
            next += 1;
            continue;
        }
        if next == 7 {
            next = PhyMode::Auto as u8;
            break;
        }
        if modecaps & (1 << next) != 0 {
            break;
        }
        next += 1;
    }
    PhyMode::try_from(next).unwrap_or(PhyMode::Auto)
}

impl TryFrom<u8> for PhyMode {
    type Error = ();
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Auto),
            1 => Ok(Self::A),
            2 => Ok(Self::B),
            3 => Ok(Self::G),
            4 => Ok(Self::N),
            5 => Ok(Self::Ac),
            6 => Ok(Self::Ax),
            _ => Err(()),
        }
    }
}

/// Convert an MHz frequency to the IEEE channel number.
// upstream: ieee80211.c ieee80211_mhz2ieee()
pub const fn mhz_to_ieee(freq: u32, flags: u32) -> u32 {
    if flags & CHAN_2GHZ != 0 {
        if freq == 2484 {
            14
        } else if freq < 2484 {
            freq.wrapping_sub(2407) / 5
        } else {
            15 + freq.wrapping_sub(2512) / 20
        }
    } else if flags & CHAN_5GHZ != 0 {
        freq.wrapping_sub(5000) / 5
    } else if freq == 2484 {
        14
    } else if freq < 2484 {
        freq.wrapping_sub(2407) / 5
    } else if freq < 5000 {
        15 + freq.wrapping_sub(2512) / 20
    } else {
        freq.wrapping_sub(5000) / 5
    }
}

/// Convert an IEEE channel number to its MHz frequency.
// upstream: ieee80211.c ieee80211_ieee2mhz()
pub const fn ieee_to_mhz(channel: u32, flags: u32) -> u32 {
    if flags & CHAN_2GHZ != 0 {
        if channel == 14 {
            2484
        } else if channel < 14 {
            2407 + channel * 5
        } else {
            2512 + (channel - 15) * 20
        }
    } else if flags & CHAN_5GHZ != 0 {
        5000 + channel * 5
    } else if channel == 14 {
        2484
    } else if channel < 14 {
        2407 + channel * 5
    } else if channel < 27 {
        2512 + (channel - 15) * 20
    } else {
        5000 + channel * 5
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelRef {
    Index(usize),
    Any,
    Invalid,
}

/// Map the channel-array pointer used by net80211 to an IEEE channel index.
// upstream: ieee80211.c ieee80211_chan2ieee()
pub fn channel_ref_to_ieee(channel: ChannelRef, max_channel: usize) -> Option<u16> {
    match channel {
        ChannelRef::Index(index) if index <= max_channel && index <= u16::MAX as usize => {
            Some(index as u16)
        }
        ChannelRef::Any => Some(u16::MAX),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequencies_follow_band_specific_and_guess_rules() {
        assert_eq!(mhz_to_ieee(2484, CHAN_2GHZ), 14);
        assert_eq!(mhz_to_ieee(2412, CHAN_2GHZ), 1);
        assert_eq!(mhz_to_ieee(5180, CHAN_5GHZ), 36);
        assert_eq!(mhz_to_ieee(5180, 0), 36);
        assert_eq!(ieee_to_mhz(14, CHAN_2GHZ), 2484);
        assert_eq!(ieee_to_mhz(36, CHAN_5GHZ), 5180);
        assert_eq!(ieee_to_mhz(15, 0), 2512);
        assert_eq!(ieee_to_mhz(36, 0), 5180);
    }

    #[test]
    fn channel_array_index_and_any_sentinel_are_checked() {
        assert_eq!(channel_ref_to_ieee(ChannelRef::Index(14), 255), Some(14));
        assert_eq!(channel_ref_to_ieee(ChannelRef::Any, 255), Some(u16::MAX));
        assert_eq!(channel_ref_to_ieee(ChannelRef::Index(256), 255), None);
        assert_eq!(channel_ref_to_ieee(ChannelRef::Invalid, 255), None);
    }

    #[test]
    fn initialization_and_mode_selection_follow_channel_flags() {
        let mut channels = [
            NetChannel::default(),
            NetChannel {
                frequency_mhz: 2412,
                flags: NET_CHAN_B | NET_CHAN_PURE_G | NET_CHAN_HT,
                extended_flags: 0,
            },
            NetChannel {
                frequency_mhz: 5180,
                flags: NET_CHAN_A | NET_CHAN_HT | NET_CHAN_VHT,
                extended_flags: NET_CHAN_X_HE,
            },
        ];
        let mut modecaps = 0;
        let mut current = PhyMode::Ac;
        let available = initialize_channels(&mut channels, &mut modecaps, &mut current);
        assert_eq!(available, [false, true, true]);
        assert_eq!(current, PhyMode::Ac);
        assert_ne!(modecaps & (1 << PhyMode::Ax as u8), 0);
        let selected = select_mode(&channels, modecaps, PhyMode::A, Some(1)).unwrap();
        assert_eq!(selected.active, [false, false, true]);
        assert_eq!(selected.ibss_channel, 2);
        assert!(select_mode(&channels, modecaps, PhyMode::B, Some(2)).is_ok());
        assert!(select_mode(&channels, 0, PhyMode::A, None).is_err());
    }

    #[test]
    fn scan_mode_selection_cycles_only_distinct_channel_sets() {
        assert_eq!(
            next_scan_mode(PhyMode::Auto, 1 << 1, false, false),
            PhyMode::A
        );
        assert_eq!(
            next_scan_mode(PhyMode::A, (1 << 1) | (1 << 3), false, false),
            PhyMode::G
        );
        assert_eq!(
            next_scan_mode(PhyMode::G, 0x7f, false, false),
            PhyMode::Auto
        );
        assert_eq!(next_scan_mode(PhyMode::A, 0x7f, true, false), PhyMode::Auto);
        assert_eq!(next_scan_mode(PhyMode::A, 0x7f, false, true), PhyMode::Auto);
    }

    #[test]
    fn rate_lookup_masks_basic_marker() {
        let rates = crate::RateSet::new(&[2, 0x84, 11]);
        assert_eq!(find_rate(&rates, 4), Some(1));
        assert_eq!(find_rate(&rates, 22), None);
    }

    #[test]
    fn ampdu_qos_requires_both_capabilities() {
        let mut flags = 0x10;
        configure_ampdu_tx(NET_CAP_TX_AMPDU, true, &mut flags);
        assert_eq!(flags, 0x10);
        configure_ampdu_tx(NET_CAP_TX_AMPDU | NET_CAP_QOS, true, &mut flags);
        assert_eq!(flags, 0x10 | NET_FLAG_QOS);
        configure_ampdu_tx(NET_CAP_TX_AMPDU | NET_CAP_QOS, false, &mut flags);
        assert_eq!(flags, 0x10);
    }
}
