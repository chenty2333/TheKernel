//! Software cipher-key dispatch and management/data key selection.
//!
//! Translated from `sys/net80211/ieee80211_crypto.c` rev 1.81 (ISC) and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2008 Damien
//! Bergamini <damien.bergamini@free.fr>.

use crate::{BipError, BipKey, CcmpError, CcmpKey, Cipher, TkipError, TkipKey, WepError, WepKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftwareCryptoError {
    UnsupportedCipher,
    InvalidKeyLength,
    Wep(WepError),
    Tkip(TkipError),
    Ccmp(CcmpError),
    Bip(BipError),
}

impl From<WepError> for SoftwareCryptoError {
    fn from(error: WepError) -> Self {
        Self::Wep(error)
    }
}
impl From<TkipError> for SoftwareCryptoError {
    fn from(error: TkipError) -> Self {
        Self::Tkip(error)
    }
}
impl From<CcmpError> for SoftwareCryptoError {
    fn from(error: CcmpError) -> Self {
        Self::Ccmp(error)
    }
}
impl From<BipError> for SoftwareCryptoError {
    fn from(error: BipError) -> Self {
        Self::Bip(error)
    }
}

#[derive(Default)]
pub enum SoftwareKey {
    #[default]
    Empty,
    Wep(WepKey),
    Tkip(TkipKey),
    Ccmp(CcmpKey),
    Bip(BipKey),
}

/// Return the source 802.11 key length for a supported cipher suite.
// upstream: ieee80211_crypto.c ieee80211_cipher_keylen()
pub const fn cipher_key_length(cipher: Cipher) -> usize {
    match cipher {
        Cipher::Wep40 => 5,
        Cipher::Tkip => 32,
        Cipher::Ccmp | Cipher::Bip => 16,
        Cipher::Wep104 => 13,
        Cipher::None | Cipher::UseGroup => 0,
    }
}

/// Dispatch software key setup to the cipher-specific context initializer.
// upstream: ieee80211_crypto.c ieee80211_set_key()
pub fn set_software_key(
    cipher: Cipher,
    key_id: u8,
    key_bytes: &[u8],
    station_mode: bool,
) -> Result<SoftwareKey, SoftwareCryptoError> {
    if cipher_key_length(cipher) != key_bytes.len() {
        return Err(SoftwareCryptoError::InvalidKeyLength);
    }
    let key = match cipher {
        Cipher::Wep40 | Cipher::Wep104 => SoftwareKey::Wep(WepKey::set_key(key_bytes, key_id)?),
        Cipher::Tkip => SoftwareKey::Tkip(TkipKey::set_key(key_bytes, key_id, station_mode)?),
        Cipher::Ccmp => SoftwareKey::Ccmp(CcmpKey::set_key(key_bytes, key_id)?),
        Cipher::Bip => SoftwareKey::Bip(BipKey::set_key(key_bytes, u16::from(key_id))?),
        Cipher::None | Cipher::UseGroup => return Err(SoftwareCryptoError::UnsupportedCipher),
    };
    Ok(key)
}

/// Delete the selected software context; each key type zeroizes on drop.
// upstream: ieee80211_crypto.c ieee80211_delete_key()
pub fn delete_software_key(key: &mut SoftwareKey) {
    *key = SoftwareKey::Empty;
}

/// Dispatch frame protection to WEP/TKIP/CCMP/BIP source handlers.
// upstream: ieee80211_crypto.c ieee80211_encrypt()
pub fn encrypt_software(
    key: &mut SoftwareKey,
    frame: &[u8],
    header_len: usize,
    random_iv: u32,
) -> Result<alloc::vec::Vec<u8>, SoftwareCryptoError> {
    match key {
        SoftwareKey::Wep(key) => Ok(crate::encrypt_wep(frame, header_len, key, random_iv)?),
        SoftwareKey::Tkip(key) => Ok(crate::encrypt_tkip(frame, header_len, key)?),
        SoftwareKey::Ccmp(key) => Ok(crate::encrypt_ccmp(frame, header_len, key)?),
        SoftwareKey::Bip(key) => Ok(crate::bip_encap(frame, key)?),
        SoftwareKey::Empty => Err(SoftwareCryptoError::UnsupportedCipher),
    }
}

/// Dispatch frame unprotection to the matching software cipher.
// upstream: ieee80211_crypto.c ieee80211_decrypt()
pub fn decrypt_software(
    key: &mut SoftwareKey,
    frame: &[u8],
    header_len: usize,
) -> Result<alloc::vec::Vec<u8>, SoftwareCryptoError> {
    match key {
        SoftwareKey::Wep(key) => Ok(crate::decrypt_wep(frame, header_len, key)?),
        SoftwareKey::Tkip(key) => Ok(crate::decrypt_tkip(frame, header_len, key)?),
        SoftwareKey::Ccmp(key) => Ok(crate::decrypt_ccmp(frame, header_len, key)?),
        SoftwareKey::Bip(key) => Ok(crate::bip_decap(frame, key)?),
        SoftwareKey::Empty => Err(SoftwareCryptoError::UnsupportedCipher),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySelection {
    Pairwise,
    Group(u8),
}

/// Select the source TX pairwise/group/IGTK key slot for a frame.
// upstream: ieee80211_crypto.c ieee80211_get_txkey()
pub const fn select_tx_key(
    rsn_enabled: bool,
    unicast_receiver: bool,
    use_group_cipher: bool,
    node_mfp: bool,
    igtk_id: u8,
    default_tx_key_id: u8,
) -> KeySelection {
    if rsn_enabled && unicast_receiver && !use_group_cipher {
        KeySelection::Pairwise
    } else if node_mfp {
        KeySelection::Group(igtk_id)
    } else {
        KeySelection::Group(default_tx_key_id)
    }
}

/// Select the source RX key from frame destination, IV key-id or group MMIE.
// upstream: ieee80211_crypto.c ieee80211_get_rxkey()
pub fn select_rx_key(
    frame: &[u8],
    header_len: usize,
    rsn_enabled: bool,
    use_group_cipher: bool,
) -> Option<KeySelection> {
    if frame.len() < 10 || header_len < 24 || frame.len() < header_len {
        return None;
    }
    let unicast_receiver = frame[4] & 1 == 0;
    if rsn_enabled && unicast_receiver && !use_group_cipher {
        return Some(KeySelection::Pairwise);
    }
    let is_group_management = !unicast_receiver && frame[0] & 0x0c == 0;
    if !is_group_management {
        let iv = frame.get(header_len..header_len + 4)?;
        return Some(KeySelection::Group(iv[3] >> 6));
    }
    if frame.len() < 24 + 18 {
        return None;
    }
    let mmie = &frame[frame.len() - 18..];
    if mmie[0] != 76 || mmie[1] != 16 {
        return None;
    }
    let key_id = u16::from_le_bytes([mmie[2], mmie[3]]);
    if key_id != 4 && key_id != 5 {
        return None;
    }
    Some(KeySelection::Group(key_id as u8))
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn source_key_lengths_and_pairwise_group_selection_match() {
        assert_eq!(cipher_key_length(Cipher::Wep40), 5);
        assert_eq!(cipher_key_length(Cipher::Tkip), 32);
        assert_eq!(cipher_key_length(Cipher::Ccmp), 16);
        assert_eq!(cipher_key_length(Cipher::Wep104), 13);
        assert_eq!(cipher_key_length(Cipher::Bip), 16);
        assert_eq!(cipher_key_length(Cipher::UseGroup), 0);
        assert_eq!(
            select_tx_key(true, true, false, false, 4, 1),
            KeySelection::Pairwise
        );
        assert_eq!(
            select_tx_key(true, false, false, false, 4, 1),
            KeySelection::Group(1)
        );
        assert_eq!(
            select_tx_key(true, true, true, true, 5, 1),
            KeySelection::Group(5)
        );
    }

    #[test]
    fn software_key_dispatch_runs_cipher_frame_roundtrip() {
        let mut key = set_software_key(Cipher::Ccmp, 0, &[0x22; 16], true).unwrap();
        let mut frame = vec![0u8; 24];
        frame[0] = 0x08;
        frame[1] = 0x01;
        frame[4] = 0x02;
        frame[10] = 0x02;
        frame[16] = 0x02;
        frame.extend_from_slice(b"dispatch");
        let protected = encrypt_software(&mut key, &frame, 24, 0).unwrap();
        let clear = decrypt_software(&mut key, &protected, 24).unwrap();
        assert_eq!(clear, frame);
        delete_software_key(&mut key);
        assert!(matches!(
            encrypt_software(&mut key, &frame, 24, 0),
            Err(SoftwareCryptoError::UnsupportedCipher)
        ));
    }

    #[test]
    fn receive_key_selection_checks_group_data_iv_and_mfp_mmie() {
        let mut data = vec![0u8; 32];
        data[0] = 0x08;
        data[4] = 0x01;
        data[24 + 3] = 2 << 6;
        assert_eq!(
            select_rx_key(&data, 24, false, false),
            Some(KeySelection::Group(2))
        );
        assert_eq!(
            select_rx_key(&data, 24, true, false),
            Some(KeySelection::Group(2))
        );
        let mut mfp = vec![0u8; 24 + 18];
        mfp[0] = 0xd0;
        mfp[4] = 0x01;
        let off = mfp.len() - 18;
        mfp[off..off + 4].copy_from_slice(&[76, 16, 4, 0]);
        assert_eq!(
            select_rx_key(&mfp, 24, true, false),
            Some(KeySelection::Group(4))
        );
        mfp[off + 2] = 8;
        assert_eq!(select_rx_key(&mfp, 24, true, false), None);
    }
}
