//! Station management information-element encoders from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_output.c` rev 1.148 and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2001 Atsushi Onoe;
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting; Copyright (c)
//! 2007-2009 Damien Bergamini.

use alloc::vec::Vec;

use crate::{
    EdcaAcParams, HeCapabilities, HtCapabilities, HtOperation, RATE_MAX_SIZE, RateSet,
    VhtCapabilities,
};

pub const ELEMID_SSID: u8 = 0;
pub const ELEMID_RATES: u8 = 1;
pub const ELEMID_DS_PARAMS: u8 = 3;
pub const ELEMID_ERP: u8 = 42;
pub const ELEMID_XRATES: u8 = 50;
pub const ELEMID_HT_CAPS: u8 = 45;
pub const ELEMID_HT_OPERATION: u8 = 61;
pub const ELEMID_VHT_CAPS: u8 = 191;
pub const ELEMID_EXTENSION: u8 = 255;
pub const ELEMID_EXT_HE_CAPS: u8 = 35;
pub const ELEMID_RSN: u8 = 48;
pub const ELEMID_VENDOR: u8 = 221;
pub const ELEMID_EDCA_PARAMS: u8 = 12;
pub const ELEMID_QOS_CAPABILITY: u8 = 46;
pub const WMM_IE_STA_QOSINFO_AC_MASK: u8 = 0x0f;
pub const WMM_IE_STA_QOSINFO_SP_MASK: u8 = 0x03;
pub const WMM_IE_STA_QOSINFO_SP_SHIFT: u8 = 5;
pub const CAPINFO_ESS: u16 = 0x0001;
pub const CAPINFO_IBSS: u16 = 0x0002;
pub const CAPINFO_PRIVACY: u16 = 0x0010;
pub const CAPINFO_SHORT_PREAMBLE: u16 = 0x0020;
pub const CAPINFO_SHORT_SLOTTIME: u16 = 0x0400;
pub const ERP_NON_ERP_PRESENT: u8 = 0x01;
pub const ERP_USE_PROTECTION: u8 = 0x02;
pub const ERP_BARKER_MODE: u8 = 0x04;
pub const AUTH_ALG_OPEN: u16 = 0;
pub const CIPHER_USE_GROUP: u32 = 0x01;
pub const CIPHER_WEP40: u32 = 0x02;
pub const CIPHER_TKIP: u32 = 0x04;
pub const CIPHER_CCMP: u32 = 0x08;
pub const CIPHER_WEP104: u32 = 0x10;
pub const CIPHER_BIP: u32 = 0x20;
pub const AKM_8021X: u32 = 0x01;
pub const AKM_PSK: u32 = 0x02;
pub const AKM_SHA256_8021X: u32 = 0x04;
pub const AKM_SHA256_PSK: u32 = 0x08;
pub const RSNCAP_PTKSA_RCNT_MASK: u16 = 0x000c;
pub const RSNCAP_GTKSA_RCNT_MASK: u16 = 0x0030;
pub const RSNCAP_MFPR: u16 = 0x0040;
pub const RSNCAP_MFPC: u16 = 0x0080;
pub const RSNCAP_PBAC: u16 = 0x1000;
pub const RSN_OUI: [u8; 3] = [0x00, 0x0f, 0xac];
pub const WPA_OUI: [u8; 3] = [0x00, 0x50, 0xf2];
pub const RATE_SIZE: usize = 8;
pub const SSID_MAX_LEN: usize = 32;
const AC_BE: u8 = 0;
const AC_BK: u8 = 1;
const AC_VI: u8 = 2;
const AC_VO: u8 = 3;

/// Map 802.1D user priority to EDCA AC, downgrading ACM categories for STA mode.
// upstream: ieee80211_output.c ieee80211_up_to_ac()
pub fn user_priority_to_access_category(
    user_priority: u8,
    admission_control_mandatory: [bool; 4],
    hostap_mode: bool,
) -> u8 {
    let mut access_category = match user_priority {
        1 | 2 => AC_BK,
        4 | 5 => AC_VI,
        6 | 7 => AC_VO,
        _ => AC_BE,
    };
    if hostap_mode {
        return access_category;
    }
    while access_category != AC_BK && admission_control_mandatory[access_category as usize] {
        access_category = match access_category {
            AC_BE => AC_BK,
            AC_VI => AC_BE,
            AC_VO => AC_VI,
            _ => break,
        };
    }
    access_category
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IeError {
    SsidTooLong,
    InvalidRateSet,
    NoExtendedRates,
    InvalidGroupCipher,
    InvalidGroupManagementCipher,
    MissingCapabilityData,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputOpMode {
    Ibss,
    HostAp,
    #[default]
    Station,
    Monitor,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RsnIePolicy {
    pub wpa: bool,
    pub group_cipher: u32,
    pub pairwise_ciphers: u32,
    pub akms: u32,
    pub peer_capabilities: u16,
    pub mfp_capable: bool,
    pub station_mode: bool,
    pub mfp_required: bool,
    pub pbac: bool,
    pub pmkid: Option<[u8; 16]>,
    pub group_management_cipher: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct AssocRequestConfig<'a> {
    pub ssid: &'a [u8],
    pub rates: &'a RateSet,
    pub listen_interval: u16,
    pub reassociation_bssid: Option<[u8; 6]>,
    pub channel_is_2ghz: bool,
    pub channel_is_5ghz: bool,
    pub channel_is_ac: bool,
    pub channel_is_he: bool,
    pub ht_enabled: bool,
    pub vht_enabled: bool,
    pub he_enabled: bool,
    pub he_mode_supported: bool,
    pub privacy_wep: bool,
    pub rsn_enabled: bool,
    pub peer_rsn_protocols: u32,
    pub short_preamble: bool,
    pub short_slot: bool,
    pub peer_qos: bool,
    pub qos_info: u8,
    pub rsn: Option<RsnIePolicy>,
    pub ht_caps: Option<&'a HtCapabilities>,
    pub vht_caps: Option<&'a VhtCapabilities>,
    pub he_caps: Option<&'a HeCapabilities>,
}

#[derive(Clone, Copy, Debug)]
pub struct ProbeRequestConfig<'a> {
    pub ssid: &'a [u8],
    pub rates: &'a RateSet,
    pub qos_info: u8,
    pub channel_is_5ghz: bool,
    pub channel_is_ac: bool,
    pub channel_is_he: bool,
    pub ht_enabled: bool,
    pub vht_enabled: bool,
    pub he_enabled: bool,
    pub he_mode_supported: bool,
    pub ht_caps: Option<&'a HtCapabilities>,
    pub vht_caps: Option<&'a VhtCapabilities>,
    pub he_caps: Option<&'a HeCapabilities>,
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

/// Compute and append the source Capability Information field.
// upstream: ieee80211_output.c ieee80211_add_capinfo()
pub fn append_capability_info(
    output: &mut Vec<u8>,
    opmode: OutputOpMode,
    is_2ghz: bool,
    privacy_configured: bool,
    short_preamble: bool,
    short_slot: bool,
) {
    let mut capinfo = match opmode {
        OutputOpMode::Ibss => CAPINFO_IBSS,
        OutputOpMode::HostAp => CAPINFO_ESS,
        OutputOpMode::Station | OutputOpMode::Monitor => 0,
    };
    if opmode == OutputOpMode::HostAp && privacy_configured {
        capinfo |= CAPINFO_PRIVACY;
    }
    if short_preamble && is_2ghz {
        capinfo |= CAPINFO_SHORT_PREAMBLE;
    }
    if short_slot {
        capinfo |= CAPINFO_SHORT_SLOTTIME;
    }
    output.extend_from_slice(&capinfo.to_le_bytes());
}

/// Append a DS Parameter Set element with the selected IEEE channel.
// upstream: ieee80211_output.c ieee80211_add_ds_params()
pub fn append_ds_params_ie(output: &mut Vec<u8>, channel: u8) -> Result<(), IeError> {
    append_ie(output, ELEMID_DS_PARAMS, &[channel])
}

/// Append an ERP element from associated-peer and short-preamble state.
// upstream: ieee80211_output.c ieee80211_add_erp()
pub fn append_erp_ie(
    output: &mut Vec<u8>,
    non_erp_station_present: bool,
    use_protection: bool,
    short_preamble: bool,
) -> Result<(), IeError> {
    let mut erp = 0;
    if non_erp_station_present {
        erp |= ERP_NON_ERP_PRESENT;
    }
    if use_protection {
        erp |= ERP_USE_PROTECTION;
    }
    if !short_preamble {
        erp |= ERP_BARKER_MODE;
    }
    append_ie(output, ELEMID_ERP, &[erp])
}

/// Build the (Re)Association Request fixed fields and ordered information elements.
// upstream: ieee80211_output.c ieee80211_get_assoc_req()
pub fn build_assoc_request_body(config: &AssocRequestConfig<'_>) -> Result<Vec<u8>, IeError> {
    let mut output = Vec::new();
    let mut capinfo = CAPINFO_ESS;
    if config.privacy_wep {
        capinfo |= CAPINFO_PRIVACY;
    }
    if config.short_preamble && config.channel_is_2ghz {
        capinfo |= CAPINFO_SHORT_PREAMBLE;
    }
    if config.short_slot {
        capinfo |= CAPINFO_SHORT_SLOTTIME;
    }
    output.extend_from_slice(&capinfo.to_le_bytes());
    output.extend_from_slice(&config.listen_interval.to_le_bytes());
    if let Some(bssid) = config.reassociation_bssid {
        output.extend_from_slice(&bssid);
    }
    append_ssid_ie(&mut output, config.ssid)?;
    append_supported_rates_ie(&mut output, config.rates)?;
    if config.rates.count > RATE_SIZE {
        append_extended_rates_ie(&mut output, config.rates)?;
    }
    if config.rsn_enabled && config.peer_rsn_protocols & crate::PROTO_RSN != 0 {
        let rsn = config.rsn.ok_or(IeError::MissingCapabilityData)?;
        append_rsn_ie(&mut output, &rsn)?;
    }
    let add_wme = config.peer_qos && config.ht_enabled;
    if add_wme {
        append_qos_capability_ie(&mut output, config.qos_info)?;
    }
    if config.rsn_enabled && config.peer_rsn_protocols & crate::PROTO_WPA != 0 {
        let mut wpa = config.rsn.ok_or(IeError::MissingCapabilityData)?;
        wpa.wpa = true;
        append_wpa_ie(&mut output, &wpa)?;
    }
    if config.ht_enabled {
        append_ht_caps_ie(
            &mut output,
            config.ht_caps.ok_or(IeError::MissingCapabilityData)?,
        )?;
    }
    if add_wme {
        append_wme_info_ie(&mut output, config.qos_info)?;
    }
    if config.vht_enabled && config.channel_is_5ghz && config.channel_is_ac {
        append_vht_caps_ie(
            &mut output,
            config.vht_caps.ok_or(IeError::MissingCapabilityData)?,
        )?;
    }
    let add_he =
        config.ht_enabled && config.he_enabled && config.he_mode_supported && config.channel_is_he;
    if add_he {
        append_he_caps_ie(
            &mut output,
            config.he_caps.ok_or(IeError::MissingCapabilityData)?,
        )?;
    }
    Ok(output)
}

/// Build the Probe Request IE sequence selected by the current channel and phy flags.
// upstream: ieee80211_output.c ieee80211_get_probe_req()
pub fn build_probe_request_ies(config: &ProbeRequestConfig<'_>) -> Result<Vec<u8>, IeError> {
    let mut output = Vec::new();
    append_ssid_ie(&mut output, config.ssid)?;
    append_supported_rates_ie(&mut output, config.rates)?;
    if config.rates.count > RATE_SIZE {
        append_extended_rates_ie(&mut output, config.rates)?;
    }
    if config.ht_enabled {
        append_ht_caps_ie(
            &mut output,
            config.ht_caps.ok_or(IeError::MissingCapabilityData)?,
        )?;
        append_wme_info_ie(&mut output, config.qos_info)?;
    }
    if config.vht_enabled && config.channel_is_5ghz && config.channel_is_ac {
        append_vht_caps_ie(
            &mut output,
            config.vht_caps.ok_or(IeError::MissingCapabilityData)?,
        )?;
    }
    if config.ht_enabled && config.he_enabled && config.he_mode_supported && config.channel_is_he {
        append_he_caps_ie(
            &mut output,
            config.he_caps.ok_or(IeError::MissingCapabilityData)?,
        )?;
    }
    Ok(output)
}

/// Build the open-system Authentication response body.
// upstream: ieee80211_output.c ieee80211_get_auth()
pub fn build_auth_body(sequence: u16, status: u16) -> [u8; 6] {
    let mut body = [0; 6];
    body[..2].copy_from_slice(&AUTH_ALG_OPEN.to_le_bytes());
    body[2..4].copy_from_slice(&sequence.to_le_bytes());
    body[4..].copy_from_slice(&status.to_le_bytes());
    body
}

/// Build a Deauthentication reason-code body.
// upstream: ieee80211_output.c ieee80211_get_deauth()
pub fn build_deauth_body(reason: u16) -> [u8; 2] {
    reason.to_le_bytes()
}

/// Build a Disassociation reason-code body.
// upstream: ieee80211_output.c ieee80211_get_disassoc()
pub fn build_disassoc_body(reason: u16) -> [u8; 2] {
    reason.to_le_bytes()
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

/// Build the RSN/WPA suite body, including PMKID and optional MFP fields.
// upstream: ieee80211_output.c ieee80211_add_rsn_body()
pub fn build_rsn_body(policy: &RsnIePolicy) -> Result<Vec<u8>, IeError> {
    let oui = if policy.wpa { WPA_OUI } else { RSN_OUI };
    let mut body = Vec::new();
    body.extend_from_slice(&1u16.to_le_bytes());
    let group_suite = match policy.group_cipher {
        CIPHER_WEP40 => 1,
        CIPHER_TKIP => 2,
        CIPHER_CCMP => 4,
        CIPHER_WEP104 => 5,
        _ => return Err(IeError::InvalidGroupCipher),
    };
    body.extend_from_slice(&oui);
    body.push(group_suite);

    let mut pairwise = Vec::new();
    for (mask, suite) in [(CIPHER_USE_GROUP, 0), (CIPHER_TKIP, 2), (CIPHER_CCMP, 4)] {
        if policy.pairwise_ciphers & mask != 0 {
            pairwise.extend_from_slice(&oui);
            pairwise.push(suite);
        }
    }
    body.extend_from_slice(&((pairwise.len() / 4) as u16).to_le_bytes());
    body.extend_from_slice(&pairwise);
    let mut akms = Vec::new();
    for (mask, suite) in [(AKM_8021X, 1), (AKM_PSK, 2)] {
        if policy.akms & mask != 0 {
            akms.extend_from_slice(&oui);
            akms.push(suite);
        }
    }
    if !policy.wpa {
        for (mask, suite) in [(AKM_SHA256_8021X, 5), (AKM_SHA256_PSK, 6)] {
            if policy.akms & mask != 0 {
                akms.extend_from_slice(&oui);
                akms.push(suite);
            }
        }
    }
    body.extend_from_slice(&((akms.len() / 4) as u16).to_le_bytes());
    body.extend_from_slice(&akms);
    if policy.wpa {
        return Ok(body);
    }

    let pmf =
        policy.mfp_capable && (!policy.station_mode || policy.peer_capabilities & RSNCAP_MFPC != 0);
    let mut rsn_capabilities =
        policy.peer_capabilities & (RSNCAP_PTKSA_RCNT_MASK | RSNCAP_GTKSA_RCNT_MASK);
    if pmf {
        rsn_capabilities |= RSNCAP_MFPC;
        if policy.mfp_required {
            rsn_capabilities |= RSNCAP_MFPR;
        }
    }
    if policy.pbac {
        rsn_capabilities |= RSNCAP_PBAC;
    }
    body.extend_from_slice(&rsn_capabilities.to_le_bytes());
    if let Some(pmkid) = policy.pmkid {
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&pmkid);
    }
    if !pmf {
        return Ok(body);
    }
    if policy.pmkid.is_none() {
        body.extend_from_slice(&0u16.to_le_bytes());
    }
    if policy.group_management_cipher != CIPHER_BIP {
        return Err(IeError::InvalidGroupManagementCipher);
    }
    body.extend_from_slice(&oui);
    body.push(6);
    Ok(body)
}

/// Append an RSN Information Element.
// upstream: ieee80211_output.c ieee80211_add_rsn()
pub fn append_rsn_ie(output: &mut Vec<u8>, policy: &RsnIePolicy) -> Result<(), IeError> {
    append_ie(output, ELEMID_RSN, &build_rsn_body(policy)?)
}

/// Append the vendor-specific WPA element and WPA v1 suite body.
// upstream: ieee80211_output.c ieee80211_add_wpa()
pub fn append_wpa_ie(output: &mut Vec<u8>, policy: &RsnIePolicy) -> Result<(), IeError> {
    let mut wpa = *policy;
    wpa.wpa = true;
    let mut body = Vec::from(WPA_OUI);
    body.push(1);
    body.extend_from_slice(&build_rsn_body(&wpa)?);
    append_ie(output, ELEMID_VENDOR, &body)
}

const fn edca(aifsn: u8, ecw_min: u8, ecw_max: u8, txop_limit: u16) -> EdcaAcParams {
    EdcaAcParams {
        admission_control_mandatory: false,
        aifsn,
        ecw_min,
        ecw_max,
        txop_limit,
    }
}

fn source_edca_table(mode: crate::PhyMode, access_point: bool) -> [EdcaAcParams; 4] {
    use crate::PhyMode;
    let (be, bk, vi, vo) = match (mode, access_point) {
        (PhyMode::B, false) => (
            edca(5, 10, 3, 0),
            edca(5, 10, 7, 0),
            edca(4, 5, 2, 188),
            edca(3, 4, 2, 102),
        ),
        (PhyMode::B, true) => (
            edca(4, 7, 3, 0),
            edca(5, 10, 7, 0),
            edca(3, 4, 1, 188),
            edca(2, 3, 1, 102),
        ),
        (PhyMode::Auto, _) => (
            edca(0, 0, 0, 0),
            edca(0, 0, 0, 0),
            edca(0, 0, 0, 0),
            edca(0, 0, 0, 0),
        ),
        (_, false) => (
            edca(4, 10, 3, 0),
            edca(4, 10, 7, 0),
            edca(3, 4, 2, 94),
            edca(2, 3, 2, 47),
        ),
        (_, true) => (
            edca(4, 6, 3, 0),
            edca(4, 10, 7, 0),
            edca(3, 4, 1, 94),
            edca(2, 3, 1, 47),
        ),
    };
    [be, bk, vi, vo]
}

/// Append the source EDCA Parameter Set element for the current PHY mode.
// upstream: ieee80211_output.c ieee80211_add_edca_params()
pub fn append_edca_params_ie(output: &mut Vec<u8>, mode: crate::PhyMode) -> Result<(), IeError> {
    append_edca_element(
        output,
        ELEMID_EDCA_PARAMS,
        [0, 0],
        &source_edca_table(mode, false),
    )
}

fn append_edca_element(
    output: &mut Vec<u8>,
    element_id: u8,
    qos_reserved: [u8; 2],
    table: &[EdcaAcParams; 4],
) -> Result<(), IeError> {
    let mut body = qos_reserved.to_vec();
    append_edca_records(&mut body, table);
    append_ie(output, element_id, &body)
}

fn append_edca_records(body: &mut Vec<u8>, table: &[EdcaAcParams; 4]) {
    for (aci, ac) in table.iter().enumerate() {
        body.push(
            ((aci as u8) << 5)
                | (u8::from(ac.admission_control_mandatory) << 4)
                | (ac.aifsn & 0x0f),
        );
        body.push((ac.ecw_max << 4) | (ac.ecw_min & 0x0f));
        body.extend_from_slice(&ac.txop_limit.to_le_bytes());
    }
}

/// Compute the WMM STA QoS Info byte from user U-APSD settings.
// upstream: ieee80211_output.c ieee80211_uapsd_qosinfo()
pub fn uapsd_qos_info(enabled: bool, access_categories: u8, max_service_period: u8) -> u8 {
    if !enabled {
        return 0;
    }
    (access_categories & WMM_IE_STA_QOSINFO_AC_MASK)
        | ((max_service_period & WMM_IE_STA_QOSINFO_SP_MASK) << WMM_IE_STA_QOSINFO_SP_SHIFT)
}

/// Append the one-byte QoS Capability information element.
// upstream: ieee80211_output.c ieee80211_add_qos_capability()
pub fn append_qos_capability_ie(output: &mut Vec<u8>, qos_info: u8) -> Result<(), IeError> {
    append_ie(output, ELEMID_QOS_CAPABILITY, &[qos_info])
}

/// Append the seven-byte Wi-Fi Alliance WMM Information element.
// upstream: ieee80211_output.c ieee80211_add_wme_info()
pub fn append_wme_info_ie(output: &mut Vec<u8>, qos_info: u8) -> Result<(), IeError> {
    append_ie(
        output,
        ELEMID_VENDOR,
        &[WPA_OUI[0], WPA_OUI[1], WPA_OUI[2], 2, 0, 1, qos_info],
    )
}

/// Append a Wi-Fi Alliance WMM Parameter element using the AP EDCA table.
// upstream: ieee80211_output.c ieee80211_add_wme_param()
pub fn append_wme_parameter_ie(output: &mut Vec<u8>, mode: crate::PhyMode) -> Result<(), IeError> {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&WPA_OUI);
    body.extend_from_slice(&[2, 1, 1, 0, 0]);
    append_edca_records(&mut body, &source_edca_table(mode, true));
    append_ie(output, ELEMID_VENDOR, &body)
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
    fn probe_request_selects_wmm_ht_vht_he_ies_for_channel_capabilities() {
        let rates = RateSet::new(&[2, 4, 11, 22, 12, 18, 24, 36, 48]);
        let ht = HtCapabilities::default();
        let vht = VhtCapabilities::default();
        let he = HeCapabilities::default();
        let config = ProbeRequestConfig {
            ssid: b"scan",
            rates: &rates,
            qos_info: 0x23,
            channel_is_5ghz: true,
            channel_is_ac: true,
            channel_is_he: true,
            ht_enabled: true,
            vht_enabled: true,
            he_enabled: true,
            he_mode_supported: true,
            ht_caps: Some(&ht),
            vht_caps: Some(&vht),
            he_caps: Some(&he),
        };
        let ies = build_probe_request_ies(&config).unwrap();
        let mut ids = Vec::new();
        let mut cursor = 0;
        while cursor < ies.len() {
            let len = usize::from(ies[cursor + 1]);
            ids.push(ies[cursor]);
            cursor += 2 + len;
        }
        assert_eq!(
            ids,
            [
                ELEMID_SSID,
                ELEMID_RATES,
                ELEMID_XRATES,
                ELEMID_HT_CAPS,
                ELEMID_VENDOR,
                ELEMID_VHT_CAPS,
                ELEMID_EXTENSION
            ]
        );
    }

    #[test]
    fn open_auth_deauth_and_disassoc_bodies_are_little_endian() {
        assert_eq!(build_auth_body(2, 17), [0, 0, 2, 0, 17, 0]);
        assert_eq!(build_deauth_body(0x1234), [0x34, 0x12]);
        assert_eq!(build_disassoc_body(0x5678), [0x78, 0x56]);
    }

    #[test]
    fn assoc_request_orders_fixed_fields_security_qos_and_phy_ies() {
        let rates = RateSet::new(&[2, 4, 11, 22, 12, 18, 24, 36, 48, 72]);
        let ht = HtCapabilities::default();
        let vht = VhtCapabilities::default();
        let he = HeCapabilities::default();
        let rsn = RsnIePolicy {
            group_cipher: CIPHER_CCMP,
            pairwise_ciphers: CIPHER_CCMP,
            akms: AKM_PSK,
            ..Default::default()
        };
        let request = build_assoc_request_body(&AssocRequestConfig {
            ssid: b"home",
            rates: &rates,
            listen_interval: 0x1234,
            reassociation_bssid: Some([7; 6]),
            channel_is_2ghz: false,
            channel_is_5ghz: true,
            channel_is_ac: true,
            channel_is_he: true,
            ht_enabled: true,
            vht_enabled: true,
            he_enabled: true,
            he_mode_supported: true,
            privacy_wep: true,
            rsn_enabled: true,
            peer_rsn_protocols: crate::PROTO_RSN,
            short_preamble: true,
            short_slot: true,
            peer_qos: true,
            qos_info: 0x6b,
            rsn: Some(rsn),
            ht_caps: Some(&ht),
            vht_caps: Some(&vht),
            he_caps: Some(&he),
        })
        .unwrap();
        assert_eq!(
            &request[..4],
            &(CAPINFO_ESS | CAPINFO_PRIVACY | CAPINFO_SHORT_SLOTTIME)
                .to_le_bytes()
                .into_iter()
                .chain(0x1234u16.to_le_bytes())
                .collect::<Vec<_>>()[..]
        );
        let mut ids = Vec::new();
        let mut offset = 4 + 6;
        while offset < request.len() {
            let len = usize::from(request[offset + 1]);
            ids.push(request[offset]);
            offset += len + 2;
        }
        assert_eq!(
            ids,
            [
                ELEMID_SSID,
                ELEMID_RATES,
                ELEMID_XRATES,
                ELEMID_RSN,
                ELEMID_QOS_CAPABILITY,
                ELEMID_HT_CAPS,
                ELEMID_VENDOR,
                ELEMID_VHT_CAPS,
                ELEMID_EXTENSION
            ]
        );
        assert_eq!(request[4..10], [7; 6]);
    }

    #[test]
    fn capability_ds_and_erp_encoders_match_source_bits() {
        let mut bytes = Vec::new();
        append_capability_info(&mut bytes, OutputOpMode::HostAp, true, true, true, true);
        assert_eq!(
            bytes,
            (CAPINFO_ESS | CAPINFO_PRIVACY | CAPINFO_SHORT_PREAMBLE | CAPINFO_SHORT_SLOTTIME)
                .to_le_bytes()
        );
        append_capability_info(&mut bytes, OutputOpMode::Station, false, true, true, false);
        assert_eq!(&bytes[2..], &[0, 0]);
        append_ds_params_ie(&mut bytes, 36).unwrap();
        assert_eq!(&bytes[4..], &[ELEMID_DS_PARAMS, 1, 36]);
        append_erp_ie(&mut bytes, true, true, false).unwrap();
        assert_eq!(
            &bytes[7..],
            &[
                ELEMID_ERP,
                1,
                ERP_NON_ERP_PRESENT | ERP_USE_PROTECTION | ERP_BARKER_MODE
            ]
        );
    }

    #[test]
    fn edca_qos_and_wmm_ie_wires_match_source_byte_order() {
        let mut ies = Vec::new();
        append_edca_params_ie(&mut ies, crate::PhyMode::A).unwrap();
        assert_eq!(ies.len(), 20);
        assert_eq!(&ies[..4], &[ELEMID_EDCA_PARAMS, 18, 0, 0]);
        assert_eq!(&ies[4..8], &[4, 0x3a, 0, 0]);
        let qos = uapsd_qos_info(true, 0x2b, 3);
        assert_eq!(qos, 0x6b);
        append_qos_capability_ie(&mut ies, qos).unwrap();
        assert_eq!(&ies[20..23], &[ELEMID_QOS_CAPABILITY, 1, 0x6b]);
        append_wme_info_ie(&mut ies, qos).unwrap();
        assert_eq!(
            &ies[23..32],
            &[ELEMID_VENDOR, 7, 0, 0x50, 0xf2, 2, 0, 1, 0x6b]
        );
        append_wme_parameter_ie(&mut ies, crate::PhyMode::B).unwrap();
        assert_eq!(&ies[32..34], &[ELEMID_VENDOR, 24]);
        assert_eq!(&ies[34..42], &[0, 0x50, 0xf2, 2, 1, 1, 0, 0]);
        assert_eq!(&ies[42..46], &[4, 0x37, 0, 0]);
        assert_eq!(uapsd_qos_info(false, 0xf, 3), 0);
    }

    #[test]
    fn rsn_and_wpa_encoders_emit_ordered_suite_lists_and_mfp_fields() {
        let policy = RsnIePolicy {
            group_cipher: CIPHER_CCMP,
            pairwise_ciphers: CIPHER_TKIP | CIPHER_CCMP,
            akms: AKM_PSK | AKM_SHA256_PSK,
            peer_capabilities: RSNCAP_MFPC | RSNCAP_PTKSA_RCNT_MASK,
            mfp_capable: true,
            station_mode: true,
            mfp_required: true,
            pbac: true,
            pmkid: Some([0x5a; 16]),
            group_management_cipher: CIPHER_BIP,
            ..Default::default()
        };
        let mut ies = Vec::new();
        append_rsn_ie(&mut ies, &policy).unwrap();
        let parsed = crate::parse_rsn(&ies).unwrap();
        assert_eq!(parsed.group_cipher, crate::Cipher::Ccmp);
        assert_eq!(parsed.pairwise_ciphers, CIPHER_TKIP | CIPHER_CCMP);
        assert_eq!(parsed.akms, AKM_PSK | AKM_SHA256_PSK);
        assert_eq!(
            parsed.capabilities & (RSNCAP_MFPC | RSNCAP_MFPR | RSNCAP_PBAC),
            RSNCAP_MFPC | RSNCAP_MFPR | RSNCAP_PBAC
        );
        assert_eq!(parsed.pmkids, [[0x5a; 16]]);
        assert_eq!(parsed.group_management_cipher, crate::Cipher::Bip);
        let mut wpa_policy = RsnIePolicy {
            wpa: true,
            group_cipher: CIPHER_TKIP,
            pairwise_ciphers: CIPHER_TKIP,
            akms: AKM_PSK | AKM_SHA256_PSK,
            ..policy
        };
        let mut wpa_ie = Vec::new();
        append_wpa_ie(&mut wpa_ie, &wpa_policy).unwrap();
        let wpa = crate::parse_wpa(&wpa_ie).unwrap();
        assert_eq!(wpa.group_cipher, crate::Cipher::Tkip);
        assert_eq!(wpa.akms, AKM_PSK); // WPA v1 does not carry SHA-256 AKMs.
        wpa_policy.group_management_cipher = 0;
        assert!(build_rsn_body(&wpa_policy).is_ok());
        assert_eq!(
            build_rsn_body(&RsnIePolicy::default()),
            Err(IeError::InvalidGroupCipher)
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

    #[test]
    fn user_priority_maps_and_downgrades_admission_control() {
        let expected = [AC_BE, AC_BK, AC_BK, AC_BE, AC_VI, AC_VI, AC_VO, AC_VO];
        for (up, ac) in expected.into_iter().enumerate() {
            assert_eq!(
                user_priority_to_access_category(up as u8, [false; 4], false),
                ac
            );
        }
        assert_eq!(
            user_priority_to_access_category(8, [false; 4], false),
            AC_BE
        );

        let acm = [false, false, true, true];
        assert_eq!(user_priority_to_access_category(6, acm, false), AC_BE);
        assert_eq!(user_priority_to_access_category(4, acm, false), AC_BE);
        assert_eq!(user_priority_to_access_category(3, acm, false), AC_BE);
        assert_eq!(user_priority_to_access_category(6, acm, true), AC_VO);

        let all_acm = [true; 4];
        assert_eq!(user_priority_to_access_category(0, all_acm, false), AC_BK);
        assert_eq!(user_priority_to_access_category(7, all_acm, false), AC_BK);
    }
}
