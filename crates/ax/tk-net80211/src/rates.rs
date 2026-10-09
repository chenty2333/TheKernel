//! Legacy IEEE 802.11 rate sets from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211.c` rev 1.92 and
//! `ieee80211_node.h` rev 1.64 (BSD-3-Clause). Copyright (c) 2001 Atsushi
//! Onoe. Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting.

pub const RATE_BASIC: u8 = 0x80;
pub const RATE_VALUE: u8 = 0x7f;
pub const RATE_MAX_SIZE: usize = 15;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateSet {
    pub count: usize,
    pub rates: [u8; RATE_MAX_SIZE],
}

impl RateSet {
    pub const fn new(rates: &[u8]) -> Self {
        let mut value = Self {
            count: rates.len(),
            rates: [0; RATE_MAX_SIZE],
        };
        let mut index = 0;
        while index < rates.len() && index < RATE_MAX_SIZE {
            value.rates[index] = rates[index];
            index += 1;
        }
        value
    }
}

pub const STANDARD_RATES_11A: RateSet = RateSet::new(&[12, 18, 24, 36, 48, 72, 96, 108]);
pub const STANDARD_RATES_11B: RateSet = RateSet::new(&[2, 4, 11, 22]);
pub const STANDARD_RATES_11G: RateSet =
    RateSet::new(&[2, 4, 11, 22, 12, 18, 24, 36, 48, 72, 96, 108]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PhyMode {
    Auto = 0,
    A    = 1,
    B    = 2,
    G    = 3,
    N    = 4,
    Ac   = 5,
    Ax   = 6,
}

const BASIC_RATES: [RateSet; 7] = [
    RateSet::new(&[]),
    RateSet::new(&[12, 24, 48]),
    RateSet::new(&[2, 4]),
    RateSet::new(&[2, 4, 11, 22]),
    RateSet::new(&[]),
    RateSet::new(&[]),
    RateSet::new(&[]),
];

/// Mark each mode's mandatory/basic rates without changing the advertised rates.
// upstream: ieee80211.c ieee80211_setbasicrates()
pub fn set_basic_rates(supported: &mut [RateSet; 7]) {
    for (mode, rates) in supported.iter_mut().enumerate() {
        let count = rates.count.min(RATE_MAX_SIZE);
        for rate in &mut rates.rates[..count] {
            *rate &= RATE_VALUE;
            if BASIC_RATES[mode].rates[..BASIC_RATES[mode].count].contains(rate) {
                *rate |= RATE_BASIC;
            }
        }
    }
}

/// Return the minimum negotiated basic rate (half-Mbit/s units).
// upstream: ieee80211.c ieee80211_min_basic_rate()
pub fn min_basic_rate(negotiated: &RateSet, is_2ghz: bool) -> u8 {
    negotiated.rates[..negotiated.count.min(RATE_MAX_SIZE)]
        .iter()
        .filter(|rate| **rate & RATE_BASIC != 0)
        .map(|rate| *rate & RATE_VALUE)
        .min()
        .unwrap_or(if is_2ghz { 2 } else { 12 })
}

/// Return the maximum negotiated basic rate, using the source band default.
// upstream: ieee80211.c ieee80211_max_basic_rate()
pub fn max_basic_rate(negotiated: &RateSet, is_2ghz: bool) -> u8 {
    negotiated.rates[..negotiated.count.min(RATE_MAX_SIZE)]
        .iter()
        .filter(|rate| **rate & RATE_BASIC != 0)
        .map(|rate| *rate & RATE_VALUE)
        .max()
        .unwrap_or(if is_2ghz { 2 } else { 12 })
        .max(if is_2ghz { 2 } else { 12 })
}

/// Convert half-Mbit/s rate to the PLCP SIGNAL octet.
// upstream: ieee80211.c ieee80211_rate2plcp()
pub fn rate_to_plcp(rate: u8, mode: PhyMode) -> u8 {
    let rate = rate & RATE_VALUE;
    match mode {
        PhyMode::B => match rate {
            2 => 10,
            4 => 20,
            11 => 55,
            22 => 110,
            44 => 220,
            _ => 0,
        },
        PhyMode::A | PhyMode::G => match rate {
            12 => 0x0b,
            18 => 0x0f,
            24 => 0x0a,
            36 => 0x0e,
            48 => 0x09,
            72 => 0x0d,
            96 => 0x08,
            108 => 0x0c,
            _ => 0,
        },
        PhyMode::Auto | PhyMode::N | PhyMode::Ac | PhyMode::Ax => 0,
    }
}

/// Convert PLCP SIGNAL to the half-Mbit/s rate.
// upstream: ieee80211.c ieee80211_plcp2rate()
pub fn plcp_to_rate(plcp: u8, mode: PhyMode) -> u8 {
    match mode {
        PhyMode::B => match plcp {
            10 => 2,
            20 => 4,
            55 => 11,
            110 => 22,
            220 => 44,
            _ => 0,
        },
        PhyMode::A | PhyMode::G => match plcp {
            0x0b => 12,
            0x0f => 18,
            0x0a => 24,
            0x0e => 36,
            0x09 => 48,
            0x0d => 72,
            0x08 => 96,
            0x0c => 108,
            _ => 0,
        },
        PhyMode::Auto | PhyMode::N | PhyMode::Ac | PhyMode::Ax => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_rate_sets_and_basic_flags_match_source() {
        assert_eq!(STANDARD_RATES_11A.count, 8);
        assert_eq!(STANDARD_RATES_11B.count, 4);
        assert_eq!(STANDARD_RATES_11G.count, 12);
        let mut supported = [STANDARD_RATES_11G; 7];
        set_basic_rates(&mut supported);
        assert_eq!(&supported[3].rates[..4], &[0x82, 0x84, 0x8b, 0x96]);
        assert_eq!(supported[4].rates[0], 2);
    }

    #[test]
    fn non_legacy_phy_modes_have_no_legacy_plcp_mapping() {
        for mode in [PhyMode::Auto, PhyMode::N, PhyMode::Ac, PhyMode::Ax] {
            assert_eq!(rate_to_plcp(24, mode), 0);
            assert_eq!(plcp_to_rate(0x0a, mode), 0);
        }
    }

    #[test]
    fn negotiated_basic_rate_bounds_use_band_defaults() {
        let rates = RateSet::new(&[18, 0x8c, 0x82, 0x96]);
        assert_eq!(min_basic_rate(&rates, true), 2);
        assert_eq!(max_basic_rate(&rates, true), 22);
        assert_eq!(min_basic_rate(&RateSet::default(), false), 12);
        assert_eq!(max_basic_rate(&RateSet::default(), false), 12);
    }

    #[test]
    fn plcp_encodings_are_inverse_for_legacy_modes() {
        for (mode, rate) in [
            (PhyMode::B, 22),
            (PhyMode::B, 44),
            (PhyMode::A, 108),
            (PhyMode::G, 12),
        ] {
            let signal = rate_to_plcp(rate, mode);
            assert_eq!(plcp_to_rate(signal, mode), rate);
        }
    }
}
