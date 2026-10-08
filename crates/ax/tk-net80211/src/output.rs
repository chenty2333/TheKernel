//! Station management information-element encoders from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_output.c` rev 1.148 and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2001 Atsushi Onoe;
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting; Copyright (c)
//! 2007-2009 Damien Bergamini.

use alloc::vec::Vec;

use crate::{HeCapabilities, HtCapabilities, HtOperation, RATE_MAX_SIZE, RateSet, VhtCapabilities};

pub const ELEMID_SSID: u8 = 0;
pub const ELEMID_RATES: u8 = 1;
pub const ELEMID_XRATES: u8 = 50;
pub const ELEMID_HT_CAPS: u8 = 45;
pub const ELEMID_HT_OPERATION: u8 = 61;
pub const ELEMID_VHT_CAPS: u8 = 191;
pub const ELEMID_EXTENSION: u8 = 255;
pub const ELEMID_EXT_HE_CAPS: u8 = 35;
pub const RATE_SIZE: usize = 8;
pub const SSID_MAX_LEN: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IeError {
    SsidTooLong,
    InvalidRateSet,
    NoExtendedRates,
}

fn append_ie(output: &mut Vec<u8>, id: u8, payload: &[u8]) -> Result<(), IeError> {
    if payload.len() > u8::MAX as usize {
        return Err(IeError::InvalidRateSet);
    }
    output.push(id);
    output.push(payload.len() as u8);
    output.extend_from_slice(payload);
    Ok(())
}

/// Append an SSID information element, including the zero-length hidden SSID form.
// upstream: ieee80211_output.c ieee80211_add_ssid()
pub fn append_ssid_ie(output: &mut Vec<u8>, ssid: &[u8]) -> Result<(), IeError> {
    if ssid.len() > SSID_MAX_LEN {
        return Err(IeError::SsidTooLong);
    }
    append_ie(output, ELEMID_SSID, ssid)
}

/// Append the first eight supported rates to the Supported Rates element.
// upstream: ieee80211_output.c ieee80211_add_rates()
pub fn append_supported_rates_ie(output: &mut Vec<u8>, rates: &RateSet) -> Result<(), IeError> {
    if rates.count > RATE_MAX_SIZE {
        return Err(IeError::InvalidRateSet);
    }
    append_ie(
        output,
        ELEMID_RATES,
        &rates.rates[..rates.count.min(RATE_SIZE)],
    )
}

/// Append rates after the first eight to Extended Supported Rates.
// upstream: ieee80211_output.c ieee80211_add_xrates()
pub fn append_extended_rates_ie(output: &mut Vec<u8>, rates: &RateSet) -> Result<(), IeError> {
    if rates.count > RATE_MAX_SIZE {
        return Err(IeError::InvalidRateSet);
    }
    if rates.count <= RATE_SIZE {
        return Err(IeError::NoExtendedRates);
    }
    append_ie(output, ELEMID_XRATES, &rates.rates[RATE_SIZE..rates.count])
}

/// Append the 26-byte HT Capabilities element.
// upstream: ieee80211_output.c ieee80211_add_htcaps()
pub fn append_ht_caps_ie(output: &mut Vec<u8>, caps: &HtCapabilities) -> Result<(), IeError> {
    let mut body = Vec::with_capacity(26);
    body.extend_from_slice(&caps.caps.to_le_bytes());
    body.push(caps.ampdu_param);
    body.extend_from_slice(&caps.rx_mcs);
    body.extend_from_slice(&(caps.max_rx_rate & 0x03ff).to_le_bytes());
    body.push(caps.tx_mcs_set);
    body.extend_from_slice(&[0; 3]);
    body.extend_from_slice(&caps.tx_caps.to_le_bytes());
    body.extend_from_slice(&caps.tx_beamforming_caps.to_le_bytes());
    body.push(caps.antenna_selection_caps);
    append_ie(output, ELEMID_HT_CAPS, &body)
}

/// Append the 22-byte HT Operation element; Basic MCS is reserved in this source builder.
// upstream: ieee80211_output.c ieee80211_add_htop()
pub fn append_ht_operation_ie(
    output: &mut Vec<u8>,
    channel: u8,
    operation: &HtOperation,
) -> Result<(), IeError> {
    let mut body = Vec::with_capacity(22);
    body.push(channel);
    body.push(operation.htop0);
    body.extend_from_slice(&operation.htop1.to_le_bytes());
    body.extend_from_slice(&operation.htop2.to_le_bytes());
    body.extend_from_slice(&[0; 16]);
    append_ie(output, ELEMID_HT_OPERATION, &body)
}

/// Append the 12-byte VHT Capabilities element.
// upstream: ieee80211_output.c ieee80211_add_vhtcaps()
pub fn append_vht_caps_ie(output: &mut Vec<u8>, caps: &VhtCapabilities) -> Result<(), IeError> {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&caps.caps.to_le_bytes());
    body.extend_from_slice(&caps.rx_mcs.to_le_bytes());
    body.extend_from_slice(&caps.rx_max_lgi_mbps.to_le_bytes());
    body.extend_from_slice(&caps.tx_mcs.to_le_bytes());
    body.extend_from_slice(&caps.tx_max_lgi_mbps.to_le_bytes());
    append_ie(output, ELEMID_VHT_CAPS, &body)
}

/// Append HE Capabilities as an Extension IE with width-dependent MCS/NSS maps.
// upstream: ieee80211_output.c ieee80211_add_hecaps()
pub fn append_he_caps_ie(output: &mut Vec<u8>, caps: &HeCapabilities) -> Result<(), IeError> {
    let phycap0 = caps.phy_caps[0];
    let has_160 = phycap0 & crate::HE_PHYCAP0_CHAN_WIDTH_160_IN_5G != 0;
    let has_80p80 = phycap0 & crate::HE_PHYCAP0_CHAN_WIDTH_8080_IN_5G != 0;
    let mcs_len = 4 + if has_160 { 4 } else { 0 } + if has_80p80 { 4 } else { 0 };
    let mut body = Vec::with_capacity(1 + crate::HE_FIXED_CAPS_LEN + mcs_len);
    body.push(ELEMID_EXT_HE_CAPS);
    body.extend_from_slice(&caps.mac_caps);
    body.extend_from_slice(&caps.phy_caps);
    body.extend_from_slice(&caps.rx_mcs_80.to_le_bytes());
    body.extend_from_slice(&caps.tx_mcs_80.to_le_bytes());
    if has_160 {
        body.extend_from_slice(&caps.rx_mcs_160.to_le_bytes());
        body.extend_from_slice(&caps.tx_mcs_160.to_le_bytes());
    }
    if has_80p80 {
        body.extend_from_slice(&caps.rx_mcs_80p80.to_le_bytes());
        body.extend_from_slice(&caps.tx_mcs_80p80.to_le_bytes());
    }
    append_ie(output, ELEMID_EXTENSION, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssid_and_rate_elements_keep_ie_ids_lengths_and_segment_boundary() {
        let mut ies = Vec::new();
        append_ssid_ie(&mut ies, b"home").unwrap();
        let rates = RateSet::new(&[2, 4, 11, 22, 12, 18, 24, 36, 48, 72, 96, 108]);
        append_supported_rates_ie(&mut ies, &rates).unwrap();
        append_extended_rates_ie(&mut ies, &rates).unwrap();
        assert_eq!(&ies[..6], &[ELEMID_SSID, 4, b'h', b'o', b'm', b'e']);
        assert_eq!(
            &ies[6..16],
            &[ELEMID_RATES, 8, 2, 4, 11, 22, 12, 18, 24, 36]
        );
        assert_eq!(&ies[16..], &[ELEMID_XRATES, 4, 48, 72, 96, 108]);
    }

    #[test]
    fn hidden_ssid_and_invalid_rate_sets_are_bounded() {
        let mut ies = Vec::new();
        append_ssid_ie(&mut ies, &[]).unwrap();
        assert_eq!(ies, [ELEMID_SSID, 0]);
        assert_eq!(
            append_ssid_ie(&mut ies, &[0; 33]),
            Err(IeError::SsidTooLong)
        );
        assert_eq!(
            append_extended_rates_ie(&mut ies, &RateSet::new(&[2; 8])),
            Err(IeError::NoExtendedRates)
        );
        let invalid = RateSet {
            count: RATE_MAX_SIZE + 1,
            ..RateSet::default()
        };
        assert_eq!(
            append_supported_rates_ie(&mut ies, &invalid),
            Err(IeError::InvalidRateSet)
        );
    }

    #[test]
    fn ht_vht_and_width_dependent_he_caps_match_ie_wire_sizes() {
        let mut out = Vec::new();
        let ht = HtCapabilities {
            caps: 0x1234,
            ampdu_param: 3,
            rx_mcs: [0x5a; 10],
            max_rx_rate: 0xffff,
            tx_mcs_set: 1,
            tx_caps: 0x4567,
            tx_beamforming_caps: 0x89ab_cdef,
            antenna_selection_caps: 5,
            ..Default::default()
        };
        append_ht_caps_ie(&mut out, &ht).unwrap();
        assert_eq!(&out[..2], &[ELEMID_HT_CAPS, 26]);
        assert_eq!(&out[2..4], &[0x34, 0x12]);
        assert_eq!(&out[15..17], &[0xff, 0x03]);
        let op = HtOperation {
            htop0: 1,
            htop1: 0x1234,
            htop2: 0x5678,
            basic_mcs: [0xff; 16],
            ..Default::default()
        };
        append_ht_operation_ie(&mut out, 36, &op).unwrap();
        assert_eq!(&out[28..30], &[ELEMID_HT_OPERATION, 22]);
        assert!(out[36..].iter().all(|byte| *byte == 0));
        let vht = VhtCapabilities {
            caps: 0x1234_5678,
            rx_mcs: 1,
            rx_max_lgi_mbps: 2,
            tx_mcs: 3,
            tx_max_lgi_mbps: 4,
            ..Default::default()
        };
        append_vht_caps_ie(&mut out, &vht).unwrap();
        assert_eq!(out[52], ELEMID_VHT_CAPS);
        assert_eq!(out[53], 12);
        let mut he = HeCapabilities::default();
        he.phy_caps[0] = crate::HE_PHYCAP0_CHAN_WIDTH_160_IN_5G;
        he.rx_mcs_80 = 1;
        he.tx_mcs_80 = 2;
        he.rx_mcs_160 = 3;
        he.tx_mcs_160 = 4;
        let start = out.len();
        append_he_caps_ie(&mut out, &he).unwrap();
        assert_eq!(out[start], ELEMID_EXTENSION);
        assert_eq!(out[start + 1], 1 + 17 + 8);
        assert_eq!(out[start + 2], ELEMID_EXT_HE_CAPS);
    }
}
