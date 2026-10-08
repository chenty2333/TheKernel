//! Station management information-element encoders from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_output.c` rev 1.148 and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2001 Atsushi Onoe;
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting; Copyright (c)
//! 2007-2009 Damien Bergamini.

use alloc::vec::Vec;

use crate::{RATE_MAX_SIZE, RateSet};

pub const ELEMID_SSID: u8 = 0;
pub const ELEMID_RATES: u8 = 1;
pub const ELEMID_XRATES: u8 = 50;
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
}
