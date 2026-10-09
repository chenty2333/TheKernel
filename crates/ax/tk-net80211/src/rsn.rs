//! RSN/WPA suite parsing translated from OpenBSD net80211.
//!
//! Upstream: OpenBSD `sys/net80211/ieee80211_input.c` revision 1.263,
//! `ieee80211_parse_rsn_cipher()`, `ieee80211_parse_rsn_akm()`,
//! `ieee80211_parse_rsn_body()`, `ieee80211_parse_rsn()`, and
//! `ieee80211_parse_wpa()`. BSD-3-Clause. Copyright (c) 2001 Atsushi Onoe;
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting; Copyright (c)
//! 2007-2009 Damien Bergamini.

use alloc::vec::Vec;

const RSN_ELEMENT_ID: u8 = 48;
const VENDOR_ELEMENT_ID: u8 = 221;
const RSN_OUI: [u8; 3] = [0x00, 0x0f, 0xac];
const MICROSOFT_OUI: [u8; 3] = [0x00, 0x50, 0xf2];
const PMKID_LEN: usize = 16;

/// OpenBSD cipher suite mask bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Cipher {
    None     = 0,
    UseGroup = 1,
    Wep40    = 2,
    Tkip     = 4,
    Ccmp     = 8,
    Wep104   = 16,
    Bip      = 32,
}

/// OpenBSD AKM suite mask bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Akm {
    None            = 0,
    Ieee8021x       = 1,
    Psk             = 2,
    Sha256Ieee8021x = 4,
    Sha256Psk       = 8,
    Sae             = 16,
}

/// Suite selection and optional RSN capabilities advertised by an AP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsnParams {
    pub group_cipher: Cipher,
    pub pairwise_cipher_count: u16,
    pub pairwise_ciphers: u32,
    pub akm_count: u16,
    pub akms: u32,
    pub group_management_cipher: Cipher,
    pub capabilities: u16,
    pub pmkids: Vec<[u8; PMKID_LEN]>,
}

/// IEEE 802.11 status values returned by OpenBSD's parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RsnStatus {
    Success            = 0,
    InvalidIe          = 40,
    BadGroupCipher     = 41,
    BadPairwiseCipher  = 42,
    UnsupportedVersion = 44,
}

impl Default for RsnParams {
    fn default() -> Self {
        Self {
            group_cipher: Cipher::Ccmp,
            pairwise_cipher_count: 1,
            pairwise_ciphers: Cipher::Ccmp as u32,
            akm_count: 1,
            akms: Akm::Ieee8021x as u32,
            group_management_cipher: Cipher::Bip,
            capabilities: 0,
            pmkids: Vec::new(),
        }
    }
}

/// Decode a four-byte RSN or WPA cipher suite selector.
// upstream: ieee80211_input.c ieee80211_parse_rsn_cipher()
pub fn parse_cipher(selector: &[u8; 4]) -> Cipher {
    let suite = selector[3];
    if selector[..3] == MICROSOFT_OUI || selector[..3] == RSN_OUI {
        match suite {
            0 => Cipher::UseGroup,
            1 => Cipher::Wep40,
            2 => Cipher::Tkip,
            4 => Cipher::Ccmp,
            5 => Cipher::Wep104,
            6 if selector[..3] == RSN_OUI => Cipher::Bip,
            _ => Cipher::None,
        }
    } else {
        Cipher::None
    }
}

/// Decode a four-byte RSN or WPA AKM suite selector.
// upstream: ieee80211_input.c ieee80211_parse_rsn_akm()
pub fn parse_akm(selector: &[u8; 4]) -> Akm {
    let suite = selector[3];
    if selector[..3] == MICROSOFT_OUI || selector[..3] == RSN_OUI {
        match suite {
            1 => Akm::Ieee8021x,
            2 => Akm::Psk,
            5 if selector[..3] == RSN_OUI => Akm::Sha256Ieee8021x,
            6 if selector[..3] == RSN_OUI => Akm::Sha256Psk,
            8 if selector[..3] == RSN_OUI => Akm::Sae,
            _ => Akm::None,
        }
    } else {
        Akm::None
    }
}

/// Parse an RSN element (element ID 48).
// upstream: ieee80211_input.c ieee80211_parse_rsn()
pub fn parse_rsn(element: &[u8]) -> Result<RsnParams, RsnStatus> {
    let body = element_body(element, RSN_ELEMENT_ID, 2)?;
    parse_body(body)
}

/// Parse a WPA vendor-specific element (00:50:f2:01).
// upstream: ieee80211_input.c ieee80211_parse_wpa()
pub fn parse_wpa(element: &[u8]) -> Result<RsnParams, RsnStatus> {
    let body = element_body(element, VENDOR_ELEMENT_ID, 6)?;
    if body[..4] != [0x00, 0x50, 0xf2, 0x01] {
        return Err(RsnStatus::InvalidIe);
    }
    parse_body(&body[4..])
}

// upstream: ieee80211_input.c ieee80211_parse_rsn_body()
fn parse_body(body: &[u8]) -> Result<RsnParams, RsnStatus> {
    if body.len() < 2 {
        return Err(RsnStatus::InvalidIe);
    }
    if u16::from_le_bytes([body[0], body[1]]) != 1 {
        return Err(RsnStatus::UnsupportedVersion);
    }
    let mut params = RsnParams::default();
    let mut cursor = 2usize;

    let Some(group_suite) = take_suite(body, &mut cursor) else {
        return Ok(params);
    };
    params.group_cipher = parse_cipher(&group_suite);
    if matches!(
        params.group_cipher,
        Cipher::None | Cipher::UseGroup | Cipher::Bip
    ) {
        return Err(RsnStatus::BadGroupCipher);
    }

    let Some(pairwise_count) = take_u16(body, &mut cursor) else {
        return Ok(params);
    };
    params.pairwise_cipher_count = pairwise_count;
    let pairwise_len = (pairwise_count as usize)
        .checked_mul(4)
        .ok_or(RsnStatus::InvalidIe)?;
    let pairwise = body
        .get(
            cursor
                ..cursor
                    .checked_add(pairwise_len)
                    .ok_or(RsnStatus::InvalidIe)?,
        )
        .ok_or(RsnStatus::InvalidIe)?;
    params.pairwise_ciphers = Cipher::None as u32;
    for selector in pairwise.as_chunks::<4>().0 {
        let selector = *selector;
        params.pairwise_ciphers |= parse_cipher(&selector) as u32;
    }
    cursor += pairwise_len;
    if params.pairwise_ciphers & Cipher::UseGroup as u32 != 0
        && (params.pairwise_ciphers != Cipher::UseGroup as u32
            || params.group_cipher == Cipher::Ccmp)
    {
        return Err(RsnStatus::BadPairwiseCipher);
    }

    let Some(akm_count) = take_u16(body, &mut cursor) else {
        return Ok(params);
    };
    params.akm_count = akm_count;
    let akm_len = (akm_count as usize)
        .checked_mul(4)
        .ok_or(RsnStatus::InvalidIe)?;
    let akms = body
        .get(cursor..cursor.checked_add(akm_len).ok_or(RsnStatus::InvalidIe)?)
        .ok_or(RsnStatus::InvalidIe)?;
    params.akms = Akm::None as u32;
    for selector in akms.as_chunks::<4>().0 {
        let selector = *selector;
        params.akms |= parse_akm(&selector) as u32;
    }
    cursor += akm_len;

    let Some(capabilities) = take_u16(body, &mut cursor) else {
        return Ok(params);
    };
    params.capabilities = capabilities;
    let Some(pmkid_count) = take_u16(body, &mut cursor) else {
        return Ok(params);
    };
    let pmkid_len = (pmkid_count as usize)
        .checked_mul(PMKID_LEN)
        .ok_or(RsnStatus::InvalidIe)?;
    let pmkids = body
        .get(cursor..cursor.checked_add(pmkid_len).ok_or(RsnStatus::InvalidIe)?)
        .ok_or(RsnStatus::InvalidIe)?;
    params.pmkids.reserve(pmkid_count as usize);
    for pmkid in pmkids.as_chunks::<PMKID_LEN>().0 {
        params.pmkids.push(*pmkid);
    }
    cursor += pmkid_len;

    if let Some(group_management) = take_suite(body, &mut cursor) {
        params.group_management_cipher = parse_cipher(&group_management);
        if params.group_management_cipher != Cipher::Bip {
            return Err(RsnStatus::BadGroupCipher);
        }
    }
    Ok(params)
}

fn element_body(element: &[u8], id: u8, minimum: usize) -> Result<&[u8], RsnStatus> {
    let length = *element.get(1).ok_or(RsnStatus::InvalidIe)? as usize;
    if element.first() != Some(&id) || length < minimum {
        return Err(RsnStatus::InvalidIe);
    }
    element.get(2..2 + length).ok_or(RsnStatus::InvalidIe)
}

fn take_suite(body: &[u8], cursor: &mut usize) -> Option<[u8; 4]> {
    let suite = body.get(*cursor..*cursor + 4)?.try_into().ok()?;
    *cursor += 4;
    Some(suite)
}

fn take_u16(body: &[u8], cursor: &mut usize) -> Option<u16> {
    let value = body.get(*cursor..*cursor + 2)?;
    *cursor += 2;
    Some(u16::from_le_bytes([value[0], value[1]]))
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn parses_ccmp_psk_rsn_and_defaults() {
        let mut element = vec![RSN_ELEMENT_ID, 20, 1, 0];
        element.extend_from_slice(&[0x00, 0x0f, 0xac, 4]); // Group CCMP.
        element.extend_from_slice(&1u16.to_le_bytes());
        element.extend_from_slice(&[0x00, 0x0f, 0xac, 4]); // Pairwise CCMP.
        element.extend_from_slice(&1u16.to_le_bytes());
        element.extend_from_slice(&[0x00, 0x0f, 0xac, 2]); // PSK.
        element.extend_from_slice(&0u16.to_le_bytes());
        let parsed = parse_rsn(&element).unwrap();
        assert_eq!(parsed.group_cipher, Cipher::Ccmp);
        assert_eq!(parsed.pairwise_ciphers, Cipher::Ccmp as u32);
        assert_eq!(parsed.akms, Akm::Psk as u32);
        assert_eq!(parsed.group_management_cipher, Cipher::Bip);
    }

    #[test]
    fn parses_wpa_vendor_ie_and_recognizes_sae_only_for_rsn_oui() {
        assert_eq!(parse_akm(&[0x00, 0x0f, 0xac, 8]), Akm::Sae);
        assert_eq!(parse_akm(&[0x00, 0x50, 0xf2, 8]), Akm::None);
        assert_eq!(parse_cipher(&[0x00, 0x0f, 0xac, 6]), Cipher::Bip);
        let mut ie = vec![VENDOR_ELEMENT_ID, 22];
        ie.extend_from_slice(&[0x00, 0x50, 0xf2, 1]);
        ie.extend_from_slice(&[1, 0]);
        ie.extend_from_slice(&[0x00, 0x50, 0xf2, 4]);
        ie.extend_from_slice(&1u16.to_le_bytes());
        ie.extend_from_slice(&[0x00, 0x50, 0xf2, 4]);
        ie.extend_from_slice(&1u16.to_le_bytes());
        ie.extend_from_slice(&[0x00, 0x50, 0xf2, 2]);
        assert_eq!(parse_wpa(&ie).unwrap().akms, Akm::Psk as u32);
    }

    #[test]
    fn rejects_versions_invalid_counts_and_bad_group_cipher() {
        assert_eq!(
            parse_rsn(&[RSN_ELEMENT_ID, 2, 2, 0]),
            Err(RsnStatus::UnsupportedVersion)
        );
        assert_eq!(
            parse_rsn(&[RSN_ELEMENT_ID, 1, 1]),
            Err(RsnStatus::InvalidIe)
        );
        let bad = [RSN_ELEMENT_ID, 6, 1, 0, 0, 0, 0, 0];
        assert_eq!(parse_rsn(&bad), Err(RsnStatus::BadGroupCipher));
    }
}
