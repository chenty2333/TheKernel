//! Typed ifmedia/rate conversions translated from OpenBSD net80211.
//!
//! Translated from OpenBSD `sys/net80211/ieee80211.c` rev 1.92 (BSD-3-Clause).
//! Copyright (c) 2001 Atsushi Onoe; Copyright (c) 2002, 2003 Sam Leffler,
//! Errno Consulting. The surrounding ifmedia registration/ioctl framework is
//! represented by the caller-facing typed enums here.

use crate::PhyMode;

const RATE_VALUE: u8 = 0x7f;
const HT_NUM_MCS: u8 = 77;
const VHT_NUM_MCS: u8 = 10;
const HE_NUM_MCS: u8 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaSubtype {
    Auto,
    Manual,
    None,
    Ds1,
    Ds2,
    Ds5,
    Ds11,
    Ds22,
    Ofdm6,
    Ofdm9,
    Ofdm12,
    Ofdm18,
    Ofdm24,
    Ofdm36,
    Ofdm48,
    Ofdm54,
    Ofdm72,
    HtMcs(u8),
    VhtMcs(u8),
    HeMcs(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaConversionError {
    RateUsedForMcsMode(PhyMode),
}

/// Convert an MCS index to an ifmedia-compatible typed subtype.
// upstream: ieee80211.c ieee80211_mcs2media()
pub fn mcs_to_media(mcs: i32, mode: PhyMode) -> Result<MediaSubtype, MediaConversionError> {
    let mcs = u8::try_from(mcs).ok();
    match mode {
        PhyMode::A | PhyMode::B | PhyMode::G => Err(MediaConversionError::RateUsedForMcsMode(mode)),
        PhyMode::N => Ok(mcs
            .filter(|mcs| *mcs < HT_NUM_MCS)
            .map_or(MediaSubtype::Auto, MediaSubtype::HtMcs)),
        PhyMode::Ac => Ok(mcs
            .filter(|mcs| *mcs < VHT_NUM_MCS)
            .map_or(MediaSubtype::Auto, MediaSubtype::VhtMcs)),
        PhyMode::Ax => Ok(mcs
            .filter(|mcs| *mcs < HE_NUM_MCS)
            .map_or(MediaSubtype::Auto, MediaSubtype::HeMcs)),
        PhyMode::Auto => Ok(MediaSubtype::Auto),
    }
}

/// Convert an ifmedia-compatible MCS subtype to its index.
// upstream: ieee80211.c ieee80211_media2mcs()
pub fn media_to_mcs(subtype: MediaSubtype) -> i32 {
    match subtype {
        MediaSubtype::Auto => -1,
        MediaSubtype::Manual | MediaSubtype::None => 0,
        MediaSubtype::HtMcs(index) if index < HT_NUM_MCS => i32::from(index),
        MediaSubtype::VhtMcs(index) if index < VHT_NUM_MCS => i32::from(index),
        MediaSubtype::HeMcs(index) if index < HE_NUM_MCS => i32::from(index),
        _ => -1,
    }
}

/// Convert a legacy 0.5-Mbit/s rate to an ifmedia subtype for its PHY mode.
// upstream: ieee80211.c ieee80211_rate2media()
pub fn rate_to_media(rate: u8, mode: PhyMode) -> Result<MediaSubtype, MediaConversionError> {
    let mode = match mode {
        PhyMode::A => PhyMode::A,
        PhyMode::B => PhyMode::B,
        PhyMode::Auto | PhyMode::G => PhyMode::G,
        PhyMode::N | PhyMode::Ac | PhyMode::Ax => {
            return Err(MediaConversionError::RateUsedForMcsMode(mode));
        }
    };
    let rate = rate & RATE_VALUE;
    let subtype = match (mode, rate) {
        (PhyMode::B | PhyMode::G, 2) => MediaSubtype::Ds1,
        (PhyMode::B | PhyMode::G, 4) => MediaSubtype::Ds2,
        (PhyMode::B | PhyMode::G, 11) => MediaSubtype::Ds5,
        (PhyMode::B | PhyMode::G, 22) => MediaSubtype::Ds11,
        (PhyMode::B, 44) => MediaSubtype::Ds22,
        (PhyMode::A | PhyMode::G, 12) => MediaSubtype::Ofdm6,
        (PhyMode::A | PhyMode::G, 18) => MediaSubtype::Ofdm9,
        (PhyMode::A | PhyMode::G, 24) => MediaSubtype::Ofdm12,
        (PhyMode::A | PhyMode::G, 36) => MediaSubtype::Ofdm18,
        (PhyMode::A | PhyMode::G, 48) => MediaSubtype::Ofdm24,
        (PhyMode::A | PhyMode::G, 72) => MediaSubtype::Ofdm36,
        (PhyMode::A | PhyMode::G, 96) => MediaSubtype::Ofdm48,
        (PhyMode::A | PhyMode::G, 108) => MediaSubtype::Ofdm54,
        _ => MediaSubtype::Auto,
    };
    Ok(subtype)
}

/// Convert an ifmedia rate subtype back to its IEEE 802.11 rate value.
// upstream: ieee80211.c ieee80211_media2rate()
pub fn media_to_rate(subtype: MediaSubtype) -> i16 {
    match subtype {
        MediaSubtype::Auto => -1,
        MediaSubtype::Manual | MediaSubtype::None => 0,
        MediaSubtype::Ds1 => 2,
        MediaSubtype::Ds2 => 4,
        MediaSubtype::Ds5 => 11,
        MediaSubtype::Ds11 => 22,
        MediaSubtype::Ds22 => 44,
        MediaSubtype::Ofdm6 => 12,
        MediaSubtype::Ofdm9 => 18,
        MediaSubtype::Ofdm12 => 24,
        MediaSubtype::Ofdm18 => 36,
        MediaSubtype::Ofdm24 => 48,
        MediaSubtype::Ofdm36 => 72,
        MediaSubtype::Ofdm48 => 96,
        MediaSubtype::Ofdm54 => 108,
        MediaSubtype::Ofdm72 => 144,
        MediaSubtype::HtMcs(_) | MediaSubtype::VhtMcs(_) | MediaSubtype::HeMcs(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcs_subtypes_roundtrip_and_boundaries_match_phy_families() {
        assert_eq!(mcs_to_media(76, PhyMode::N), Ok(MediaSubtype::HtMcs(76)));
        assert_eq!(mcs_to_media(77, PhyMode::N), Ok(MediaSubtype::Auto));
        assert_eq!(mcs_to_media(9, PhyMode::Ac), Ok(MediaSubtype::VhtMcs(9)));
        assert_eq!(mcs_to_media(12, PhyMode::Ax), Ok(MediaSubtype::Auto));
        assert_eq!(media_to_mcs(MediaSubtype::HtMcs(76)), 76);
        assert_eq!(media_to_mcs(MediaSubtype::HtMcs(77)), -1);
        assert_eq!(media_to_mcs(MediaSubtype::Auto), -1);
        assert_eq!(media_to_mcs(MediaSubtype::Manual), 0);
    }

    #[test]
    fn legacy_rate_mapping_preserves_basic_bit_and_mode_rules() {
        assert_eq!(rate_to_media(0x82, PhyMode::B), Ok(MediaSubtype::Ds1));
        assert_eq!(rate_to_media(108, PhyMode::A), Ok(MediaSubtype::Ofdm54));
        assert_eq!(rate_to_media(44, PhyMode::A), Ok(MediaSubtype::Auto));
        assert_eq!(media_to_rate(MediaSubtype::Ofdm72), 144);
        assert_eq!(media_to_rate(MediaSubtype::Auto), -1);
        assert_eq!(
            rate_to_media(12, PhyMode::N),
            Err(MediaConversionError::RateUsedForMcsMode(PhyMode::N))
        );
    }
}
