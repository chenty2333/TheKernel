//! 802.11 peer legacy rate negotiation inputs from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_node.c` rev 1.217,
//! `ieee80211_node.h` rev 1.64, and `ieee80211.h` rev 1.137 (BSD-3-Clause).
//! Copyright (c) 2001 Atsushi Onoe, (c) 2002, 2003 Sam Leffler, Errno
//! Consulting, and (c) 2008 Damien Bergamini.

use crate::{PhyMode, RATE_MAX_SIZE, RATE_VALUE, RateSet, STANDARD_RATES_11A};

pub const NODE_ERP: u32 = 1 << 10;
pub const CHAN_OFDM: u16 = 0x0040;
pub const CHAN_DYN: u16 = 0x0400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateIeError {
    ShortIe,
    BadIeLength,
    UnsupportedRatesIe,
    UnsupportedExtendedRatesIe,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerRateState {
    pub rates: RateSet,
    pub flags: u32,
    pub receive_rates_too_big: u32,
}

/// Detect an 11g peer by OFDM rates on a 2.4-GHz channel.
// upstream: ieee80211_node.c ieee80211_node_is_11g()
pub fn node_is_11g(peer_rates: &RateSet, channel_is_2ghz: bool) -> bool {
    channel_is_2ghz
        && peer_rates.rates[..peer_rates.count.min(RATE_MAX_SIZE)]
            .iter()
            .any(|rate| {
                STANDARD_RATES_11A.rates[..STANDARD_RATES_11A.count].contains(&(rate & RATE_VALUE))
            })
}

/// Install Supported/Extended Supported Rates elements then delegate rate fixing.
// upstream: ieee80211_node.c ieee80211_setup_rates()
pub fn setup_rates(
    peer: &mut PeerRateState,
    rates_ie: &[u8],
    extended_rates_ie: Option<&[u8]>,
    channel_is_2ghz: bool,
    fix_rate: impl FnOnce(&mut RateSet) -> i32,
) -> Result<i32, RateIeError> {
    if rates_ie.len() < 2 {
        return Err(RateIeError::ShortIe);
    }
    let rates_count = usize::from(rates_ie[1]);
    if rates_count > rates_ie.len().saturating_sub(2) {
        return Err(RateIeError::BadIeLength);
    }
    if rates_count > RATE_MAX_SIZE {
        return Err(RateIeError::UnsupportedRatesIe);
    }
    peer.rates = RateSet::default();
    peer.rates.count = rates_count;
    peer.rates.rates[..rates_count].copy_from_slice(&rates_ie[2..2 + rates_count]);
    if let Some(ext) = extended_rates_ie {
        if ext.len() < 2 {
            return Err(RateIeError::ShortIe);
        }
        let ext_count = usize::from(ext[1]);
        if ext_count > ext.len().saturating_sub(2) {
            return Err(RateIeError::BadIeLength);
        }
        let available = RATE_MAX_SIZE - peer.rates.count;
        let copied = ext_count.min(available);
        peer.rates.rates[peer.rates.count..peer.rates.count + copied]
            .copy_from_slice(&ext[2..2 + copied]);
        peer.rates.count += copied;
        if copied != ext_count {
            peer.receive_rates_too_big += 1;
        }
    }
    if node_is_11g(&peer.rates, channel_is_2ghz) {
        peer.flags |= NODE_ERP;
    }
    Ok(fix_rate(&mut peer.rates))
}

/// Resolve legacy 11a/b/g mode from fixed local mode or peer channel/rates.
// upstream: ieee80211_node.c ieee80211_node_abg_mode()
pub fn node_abg_mode(
    fixed_mode: Option<PhyMode>,
    channel_is_5ghz: bool,
    channel_flags: u16,
    node_flags: u32,
) -> PhyMode {
    match fixed_mode {
        Some(PhyMode::A) => return PhyMode::A,
        Some(PhyMode::B) => return PhyMode::B,
        Some(PhyMode::G) => {}
        _ => {}
    }
    if channel_is_5ghz {
        PhyMode::A
    } else if node_flags & NODE_ERP != 0 && channel_flags & (CHAN_OFDM | CHAN_DYN) != 0 {
        PhyMode::G
    } else {
        PhyMode::B
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_rates_are_bounded_extended_and_passed_to_fix_rate() {
        let mut peer = PeerRateState::default();
        let rates = [1, 4, 2, 4, 11, 22];
        let extended = [50, 12, 12, 18, 24, 36, 48, 72, 96, 108, 2, 4, 11, 22];
        let result = setup_rates(&mut peer, &rates, Some(&extended), true, |set| {
            assert_eq!(set.count, RATE_MAX_SIZE);
            17
        })
        .unwrap();
        assert_eq!(result, 17);
        assert_eq!(peer.rates.count, RATE_MAX_SIZE);
        assert_eq!(peer.receive_rates_too_big, 1);
        assert_ne!(peer.flags & NODE_ERP, 0);
        assert!(node_is_11g(&peer.rates, true));
    }

    #[test]
    fn malformed_rate_ie_and_abg_mode_resolution_are_explicit() {
        let mut peer = PeerRateState::default();
        assert_eq!(
            setup_rates(&mut peer, &[1, 2, 12], None, false, |_| 0),
            Err(RateIeError::BadIeLength)
        );
        assert_eq!(node_abg_mode(None, true, 0, 0), PhyMode::A);
        assert_eq!(node_abg_mode(None, false, CHAN_OFDM, NODE_ERP), PhyMode::G);
        assert_eq!(node_abg_mode(None, false, CHAN_DYN, 0), PhyMode::B);
        assert_eq!(
            node_abg_mode(Some(PhyMode::B), true, CHAN_OFDM, NODE_ERP),
            PhyMode::B
        );
    }
}
