//! Legacy rate negotiation from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_proto.c` rev 1.176,
//! `ieee80211_node.h` rev 1.64 and `ieee80211.h` rev 1.137 (BSD-3-Clause).
//! Copyright (c) 2001 Atsushi Onoe; Copyright (c) 2002, 2003 Sam Leffler,
//! Errno Consulting; Copyright (c) 2007-2009 Damien Bergamini.

use crate::{RATE_MAX_SIZE, RATE_VALUE, RateSet};

pub const FIX_RATE_SORT: u32 = 0x01;
pub const FIX_RATE_FIXED: u32 = 0x02;
pub const FIX_RATE_NEGOTIATE: u32 = 0x04;
pub const FIX_RATE_DELETE: u32 = 0x08;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixRateConfig<'a> {
    pub supported_rates: &'a RateSet,
    pub fixed_rate: Option<usize>,
    pub hostap_mode: bool,
}

/// Sort, negotiate, delete unsupported rates and validate local fixed-rate policy.
// upstream: ieee80211_proto.c ieee80211_fix_rate()
pub fn fix_rate(peer_rates: &mut RateSet, config: FixRateConfig<'_>, mut flags: u32) -> u8 {
    if flags & FIX_RATE_FIXED != 0 && config.fixed_rate.is_none() {
        flags &= !FIX_RATE_FIXED;
    }
    let supported_count = config.supported_rates.count.min(RATE_MAX_SIZE);
    let peer_count = peer_rates.count.min(RATE_MAX_SIZE);
    peer_rates.count = peer_count;
    let mut ok_rate = 0;
    let mut bad_rate = 0;
    let mut fixed_rate = 0;
    let mut error = false;
    let mut index = 0;
    while index < peer_rates.count {
        if flags & FIX_RATE_SORT != 0 {
            for next in index + 1..peer_rates.count {
                if peer_rates.rates[index] & RATE_VALUE > peer_rates.rates[next] & RATE_VALUE {
                    peer_rates.rates.swap(index, next);
                }
            }
        }
        let rate = peer_rates.rates[index] & RATE_VALUE;
        bad_rate = rate;
        if flags & FIX_RATE_FIXED != 0 {
            if let Some(fixed_index) = config.fixed_rate {
                if fixed_index < supported_count
                    && rate == config.supported_rates.rates[fixed_index] & RATE_VALUE
                {
                    fixed_rate = rate;
                }
            }
        }
        let mut ignore = false;
        if flags & FIX_RATE_NEGOTIATE != 0 {
            let supported = config.supported_rates.rates[..supported_count]
                .iter()
                .find(|supported_rate| **supported_rate & RATE_VALUE == rate);
            if let Some(supported_rate) = supported {
                // Preserve the AP's Basic marker on mutually-supported rates.
                peer_rates.rates[index] = *supported_rate;
            } else {
                if config.hostap_mode && peer_rates.rates[index] & crate::LEGACY_RATE_BASIC != 0 {
                    error = true;
                }
                ignore = true;
            }
        }
        if flags & FIX_RATE_DELETE != 0 && ignore {
            peer_rates.count -= 1;
            for move_index in index..peer_rates.count {
                peer_rates.rates[move_index] = peer_rates.rates[move_index + 1];
            }
            peer_rates.rates[peer_rates.count] = 0;
            continue;
        }
        if !ignore {
            ok_rate = peer_rates.rates[index];
        }
        index += 1;
    }
    if ok_rate == 0 || error || (flags & FIX_RATE_FIXED != 0 && fixed_rate == 0) {
        bad_rate | crate::LEGACY_RATE_BASIC
    } else {
        ok_rate & RATE_VALUE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> RateSet {
        RateSet::new(&[0x82, 0x84, 11, 22, 0x8c, 18, 24, 36])
    }

    #[test]
    fn negotiation_sorts_overwrites_basic_bits_and_deletes_unsupported() {
        let mut peer = RateSet::new(&[10, 0x8c, 18, 0x82]);
        let result = fix_rate(
            &mut peer,
            FixRateConfig {
                supported_rates: &local(),
                fixed_rate: None,
                hostap_mode: false,
            },
            FIX_RATE_SORT | FIX_RATE_NEGOTIATE | FIX_RATE_DELETE,
        );
        assert_eq!(result, 18);
        assert_eq!(peer.count, 3);
        assert_eq!(&peer.rates[..peer.count], &[0x82, 0x8c, 18]);
    }

    #[test]
    fn unsupported_basic_hostap_and_fixed_rate_fail_closed() {
        let mut peer = RateSet::new(&[0x80 | 10, 12]);
        assert_eq!(
            fix_rate(
                &mut peer,
                FixRateConfig {
                    supported_rates: &local(),
                    fixed_rate: None,
                    hostap_mode: true
                },
                FIX_RATE_NEGOTIATE
            ),
            12 | crate::LEGACY_RATE_BASIC
        );
        let mut station = RateSet::new(&[12]);
        let result = fix_rate(
            &mut station,
            FixRateConfig {
                supported_rates: &local(),
                fixed_rate: Some(1),
                hostap_mode: false,
            },
            FIX_RATE_FIXED | FIX_RATE_NEGOTIATE,
        );
        assert_eq!(result, 12 | crate::LEGACY_RATE_BASIC);
    }
}
