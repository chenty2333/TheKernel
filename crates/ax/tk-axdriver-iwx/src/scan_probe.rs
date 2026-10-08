//! UMAC probe-request frame and IE segment construction from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `sys/net80211/ieee80211.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

pub const PROBE_REQUEST_BYTES: usize = 512;
pub const PROBE_REQUEST_WIRE_BYTES: usize = 20 + PROBE_REQUEST_BYTES;
pub const SUPPORTED_RATES_IE: u8 = 1;
pub const DS_PARAMETER_IE: u8 = 3;
pub const EXTENDED_RATES_IE: u8 = 50;
pub const HT_CAPABILITIES_IE: u8 = 45;
pub const VHT_CAPABILITIES_IE: u8 = 191;
pub const STANDARD_RATE_IE_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeSegment {
    pub offset: u16,
    pub length: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanProbeRequest {
    pub mac_header: ProbeSegment,
    pub band_data: [ProbeSegment; 3],
    pub common_data: ProbeSegment,
    pub frame_length: u16,
    pub bytes: [u8; PROBE_REQUEST_BYTES],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeRequestError {
    RatesTooLong,
    InvalidCapabilityIe,
    BufferTooSmall,
}

#[derive(Debug, Clone, Copy)]
pub struct ProbeRequestConfig<'a> {
    pub station_address: [u8; 6],
    pub rates_2ghz: &'a [u8],
    pub rates_5ghz: &'a [u8],
    pub supports_5ghz: bool,
    pub include_ds_parameter: bool,
    pub vht_capabilities_ie: Option<&'a [u8]>,
    pub ht_capabilities_ie: Option<&'a [u8]>,
}

/// Build the shared MAC header, firmware-inserted SSID IE, per-band rate IEs,
/// and common HT capability segment used by the UMAC scan offload command.
// upstream: if_iwx.c iwx_fill_probe_req()
pub fn build_scan_probe_request(
    config: ProbeRequestConfig<'_>,
) -> Result<ScanProbeRequest, ProbeRequestError> {
    let mut request = ScanProbeRequest {
        mac_header: ProbeSegment::default(),
        band_data: [ProbeSegment::default(); 3],
        common_data: ProbeSegment::default(),
        frame_length: 0,
        bytes: [0; PROBE_REQUEST_BYTES],
    };
    // 802.11 management probe request, no-DS, broadcast DA/BSSID.
    request.bytes[0] = 0x40;
    request.bytes[1] = 0;
    request.bytes[4..10].fill(0xff);
    request.bytes[10..16].copy_from_slice(&config.station_address);
    request.bytes[16..22].fill(0xff);
    request.bytes[2..4].fill(0); // duration is hardware-filled.
    request.bytes[22..24].fill(0); // sequence is hardware-filled.
    let mut cursor = 24;
    let mut ssid_ie = Vec::new();
    tk_net80211::append_ssid_ie(&mut ssid_ie, &[])
        .map_err(|_| ProbeRequestError::BufferTooSmall)?;
    append_bytes(&mut request.bytes, &mut cursor, &ssid_ie)?; // firmware injects SSID.
    request.mac_header = ProbeSegment {
        offset: 0,
        length: cursor as u16,
    };

    let start_2g = cursor;
    append_rates(&mut request.bytes, &mut cursor, config.rates_2ghz)?;
    if config.include_ds_parameter {
        write_ie(&mut request.bytes, &mut cursor, DS_PARAMETER_IE, &[0])?;
    }
    request.band_data[0] = segment(start_2g, cursor)?;

    if config.supports_5ghz {
        let start_5g = cursor;
        append_rates(&mut request.bytes, &mut cursor, config.rates_5ghz)?;
        if let Some(vht_ie) = config.vht_capabilities_ie {
            validate_ie(vht_ie, VHT_CAPABILITIES_IE)?;
            append_bytes(&mut request.bytes, &mut cursor, vht_ie)?;
        }
        request.band_data[1] = segment(start_5g, cursor)?;
    }

    let common_start = cursor;
    if let Some(ht_ie) = config.ht_capabilities_ie {
        validate_ie(ht_ie, HT_CAPABILITIES_IE)?;
        append_bytes(&mut request.bytes, &mut cursor, ht_ie)?;
    }
    request.common_data = segment(common_start, cursor)?;
    request.frame_length = u16::try_from(cursor).map_err(|_| ProbeRequestError::BufferTooSmall)?;
    Ok(request)
}

/// Serialize the packed scan-probe descriptor followed by its fixed data block.
pub fn encode_scan_probe_request(request: &ScanProbeRequest) -> [u8; PROBE_REQUEST_WIRE_BYTES] {
    let mut bytes = [0; PROBE_REQUEST_WIRE_BYTES];
    let mut cursor = 0;
    for segment in [
        request.mac_header,
        request.band_data[0],
        request.band_data[1],
        request.band_data[2],
        request.common_data,
    ] {
        bytes[cursor..cursor + 2].copy_from_slice(&segment.offset.to_le_bytes());
        bytes[cursor + 2..cursor + 4].copy_from_slice(&segment.length.to_le_bytes());
        cursor += 4;
    }
    bytes[cursor..].copy_from_slice(&request.bytes);
    bytes
}

fn append_rates(
    buffer: &mut [u8; PROBE_REQUEST_BYTES],
    cursor: &mut usize,
    rates: &[u8],
) -> Result<(), ProbeRequestError> {
    if rates.len() > tk_net80211::RATE_MAX_SIZE {
        return Err(ProbeRequestError::RatesTooLong);
    }
    let mut rate_set = tk_net80211::RateSet::default();
    rate_set.count = rates.len();
    rate_set.rates[..rates.len()].copy_from_slice(rates);
    let mut ies = Vec::new();
    tk_net80211::append_supported_rates_ie(&mut ies, &rate_set)
        .map_err(|_| ProbeRequestError::RatesTooLong)?;
    if rates.len() > STANDARD_RATE_IE_LIMIT {
        tk_net80211::append_extended_rates_ie(&mut ies, &rate_set)
            .map_err(|_| ProbeRequestError::RatesTooLong)?;
    }
    append_bytes(buffer, cursor, &ies)
}

fn validate_ie(bytes: &[u8], expected_id: u8) -> Result<(), ProbeRequestError> {
    if bytes.len() < 2 || bytes[0] != expected_id || usize::from(bytes[1]) + 2 != bytes.len() {
        return Err(ProbeRequestError::InvalidCapabilityIe);
    }
    Ok(())
}

fn write_ie(
    buffer: &mut [u8; PROBE_REQUEST_BYTES],
    cursor: &mut usize,
    element_id: u8,
    body: &[u8],
) -> Result<(), ProbeRequestError> {
    if body.len() > u8::MAX as usize {
        return Err(ProbeRequestError::RatesTooLong);
    }
    let total = body.len() + 2;
    let end = cursor
        .checked_add(total)
        .ok_or(ProbeRequestError::BufferTooSmall)?;
    if end > buffer.len() {
        return Err(ProbeRequestError::BufferTooSmall);
    }
    buffer[*cursor] = element_id;
    buffer[*cursor + 1] = body.len() as u8;
    buffer[*cursor + 2..end].copy_from_slice(body);
    *cursor = end;
    Ok(())
}

fn append_bytes(
    buffer: &mut [u8; PROBE_REQUEST_BYTES],
    cursor: &mut usize,
    bytes: &[u8],
) -> Result<(), ProbeRequestError> {
    let end = cursor
        .checked_add(bytes.len())
        .ok_or(ProbeRequestError::BufferTooSmall)?;
    if end > buffer.len() {
        return Err(ProbeRequestError::BufferTooSmall);
    }
    buffer[*cursor..end].copy_from_slice(bytes);
    *cursor = end;
    Ok(())
}

fn segment(start: usize, end: usize) -> Result<ProbeSegment, ProbeRequestError> {
    Ok(ProbeSegment {
        offset: u16::try_from(start).map_err(|_| ProbeRequestError::BufferTooSmall)?,
        length: u16::try_from(end - start).map_err(|_| ProbeRequestError::BufferTooSmall)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_request_layouts_include_band_rates_and_shared_ht_segment() {
        let rates = [2, 4, 11, 22, 12, 18, 24, 36, 48, 72];
        let vht = [
            VHT_CAPABILITIES_IE,
            12,
            0,
            1,
            2,
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
            11,
        ];
        let mut ht = [0u8; 28];
        ht[0] = HT_CAPABILITIES_IE;
        ht[1] = 26;
        let request = build_scan_probe_request(ProbeRequestConfig {
            station_address: [0, 1, 2, 3, 4, 5],
            rates_2ghz: &rates,
            rates_5ghz: &rates[..8],
            supports_5ghz: true,
            include_ds_parameter: true,
            vht_capabilities_ie: Some(&vht),
            ht_capabilities_ie: Some(&ht),
        })
        .unwrap();
        assert_eq!(&request.bytes[..2], &[0x40, 0]);
        assert_eq!(&request.bytes[4..10], &[0xff; 6]);
        assert_eq!(&request.bytes[10..16], &[0, 1, 2, 3, 4, 5]);
        assert_eq!(&request.bytes[24..26], &[0, 0]);
        assert_eq!(request.mac_header.length, 26);
        assert_eq!(request.band_data[0].length, 2 + 8 + 2 + 2 + 3);
        assert_eq!(usize::from(request.band_data[1].length), 2 + 8 + vht.len());
        assert_eq!(request.common_data.length, ht.len() as u16);
        assert_eq!(
            usize::from(request.frame_length),
            usize::from(request.common_data.offset + request.common_data.length)
        );
        let wire = encode_scan_probe_request(&request);
        assert_eq!(wire.len(), PROBE_REQUEST_WIRE_BYTES);
        assert_eq!(&wire[..4], &[0, 0, 26, 0]);
        assert_eq!(&wire[20..22], &[0x40, 0]);
    }

    #[test]
    fn probe_request_rejects_malformed_caps_and_preserves_empty_fifth_band() {
        let invalid = [VHT_CAPABILITIES_IE, 11, 0];
        assert_eq!(
            build_scan_probe_request(ProbeRequestConfig {
                station_address: [0; 6],
                rates_2ghz: &[],
                rates_5ghz: &[],
                supports_5ghz: true,
                include_ds_parameter: false,
                vht_capabilities_ie: Some(&invalid),
                ht_capabilities_ie: None,
            }),
            Err(ProbeRequestError::InvalidCapabilityIe)
        );
        let request = build_scan_probe_request(ProbeRequestConfig {
            station_address: [0; 6],
            rates_2ghz: &[],
            rates_5ghz: &[],
            supports_5ghz: false,
            include_ds_parameter: false,
            vht_capabilities_ie: None,
            ht_capabilities_ie: None,
        })
        .unwrap();
        assert_eq!(request.band_data[1], ProbeSegment::default());
        assert_eq!(request.band_data[2], ProbeSegment::default());
    }
}
