//! IEEE channel/frequency conversions from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211.c` rev 1.92 (BSD-3-Clause).
//! Copyright (c) 2001 Atsushi Onoe. Copyright (c) 2002, 2003 Sam Leffler,
//! Errno Consulting.

pub const CHAN_2GHZ: u32 = 0x0080;
pub const CHAN_5GHZ: u32 = 0x0100;

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
}
