//! Software WEP frame crypto from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_crypto_wep.c` rev 1.13 (ISC) and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2001 Atsushi Onoe;
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting.

const IEEE80211_HEADER_LEN: usize = 24;
const WEP_IV_LEN: usize = 3;
const WEP_KEYID_LEN: usize = 1;
const WEP_HEADER_LEN: usize = WEP_IV_LEN + WEP_KEYID_LEN;
const WEP_CRC_LEN: usize = 4;
const WEP_TOTAL_LEN: usize = WEP_HEADER_LEN + WEP_CRC_LEN;
const FC1_PROTECTED: u8 = 0x40;
const CRC32_LE_POLY: u32 = 0xedb8_8320;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WepError {
    InvalidKeyLength,
    InvalidKeyId,
    InvalidHeader,
    InvalidFrame,
    InvalidIv,
    IcvMismatch,
}

/// Per-key WEP state with source IV progression and weak-IV avoidance.
pub struct WepKey {
    key: [u8; 13],
    key_len: u8,
    pub key_id: u8,
    tx_iv: u32,
}

impl WepKey {
    /// Initialize the software WEP key context.
    // upstream: ieee80211_crypto_wep.c ieee80211_wep_set_key()
    pub fn set_key(key: &[u8], key_id: u8) -> Result<Self, WepError> {
        if !matches!(key.len(), 5 | 13) {
            return Err(WepError::InvalidKeyLength);
        }
        if key_id > 3 {
            return Err(WepError::InvalidKeyId);
        }
        let mut material = [0; 13];
        material[..key.len()].copy_from_slice(key);
        Ok(Self {
            key: material,
            key_len: key.len() as u8,
            key_id,
            tx_iv: 0,
        })
    }

    /// Clear the software key and IV state.
    // upstream: ieee80211_crypto_wep.c ieee80211_wep_delete_key()
    pub fn delete_key(&mut self) {
        for byte in &mut self.key {
            // SAFETY: `byte` is valid and uniquely borrowed key material.
            unsafe { core::ptr::write_volatile(byte, 0) };
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        self.key_len = 0;
        self.key_id = 0;
        self.tx_iv = 0;
    }

    fn next_iv(&mut self, random_iv: u32) -> u32 {
        let mut iv = if self.tx_iv != 0 {
            self.tx_iv
        } else {
            random_iv
        };
        if iv >= 0x03ff00 && (iv & 0xf8ff00) == 0x00ff00 {
            iv = iv.wrapping_add(0x000100);
        }
        self.tx_iv = iv.wrapping_add(1);
        iv & 0x00ff_ffff
    }
}

impl Drop for WepKey {
    fn drop(&mut self) {
        self.delete_key();
    }
}

/// Encrypt an 802.11 frame body, append the WEP IV/key-id and encrypted ICV.
// upstream: ieee80211_crypto_wep.c ieee80211_wep_encrypt()
pub fn encrypt_wep(
    frame: &[u8],
    header_len: usize,
    key: &mut WepKey,
    random_iv: u32,
) -> Result<alloc::vec::Vec<u8>, WepError> {
    if header_len < IEEE80211_HEADER_LEN || frame.len() < header_len {
        return Err(WepError::InvalidHeader);
    }
    if !matches!(key.key_len, 5 | 13) {
        return Err(WepError::InvalidKeyLength);
    }
    if random_iv > 0x00ff_ffff {
        return Err(WepError::InvalidIv);
    }
    let iv = key.next_iv(random_iv);
    let mut seed = [0u8; 16];
    seed[0] = iv as u8;
    seed[1] = (iv >> 8) as u8;
    seed[2] = (iv >> 16) as u8;
    seed[3..3 + usize::from(key.key_len)].copy_from_slice(&key.key[..usize::from(key.key_len)]);
    let mut rc4 = Rc4::new(&seed[..3 + usize::from(key.key_len)]);
    seed.fill(0);

    let plaintext = &frame[header_len..];
    let mut crc = crc32_le_update(!0, plaintext);
    crc = !crc;
    let mut protected_body = alloc::vec::Vec::new();
    protected_body
        .try_reserve_exact(plaintext.len() + WEP_CRC_LEN)
        .map_err(|_| WepError::InvalidFrame)?;
    protected_body.extend_from_slice(plaintext);
    protected_body.extend_from_slice(&crc.to_le_bytes());
    rc4.apply(&mut protected_body);

    let mut output = alloc::vec::Vec::new();
    let output_len = frame
        .len()
        .checked_add(WEP_TOTAL_LEN)
        .ok_or(WepError::InvalidFrame)?;
    output
        .try_reserve_exact(output_len)
        .map_err(|_| WepError::InvalidFrame)?;
    output.extend_from_slice(&frame[..header_len]);
    output[1] |= FC1_PROTECTED;
    output.extend_from_slice(&[iv as u8, (iv >> 8) as u8, (iv >> 16) as u8, key.key_id << 6]);
    output.extend_from_slice(&protected_body);
    Ok(output)
}

/// Decrypt WEP body, verify its little-endian ICV, and clear Protected.
// upstream: ieee80211_crypto_wep.c ieee80211_wep_decrypt()
pub fn decrypt_wep(
    frame: &[u8],
    header_len: usize,
    key: &WepKey,
) -> Result<alloc::vec::Vec<u8>, WepError> {
    if header_len < IEEE80211_HEADER_LEN || frame.len() < header_len + WEP_TOTAL_LEN {
        return Err(WepError::InvalidFrame);
    }
    if !matches!(key.key_len, 5 | 13) {
        return Err(WepError::InvalidKeyLength);
    }
    let iv_header = &frame[header_len..header_len + WEP_HEADER_LEN];
    if iv_header[3] >> 6 != key.key_id {
        return Err(WepError::InvalidKeyId);
    }
    let mut seed = [0u8; 16];
    seed[..3].copy_from_slice(&iv_header[..3]);
    seed[3..3 + usize::from(key.key_len)].copy_from_slice(&key.key[..usize::from(key.key_len)]);
    let mut rc4 = Rc4::new(&seed[..3 + usize::from(key.key_len)]);
    seed.fill(0);
    let mut clear = alloc::vec::Vec::new();
    clear
        .try_reserve_exact(frame.len() - header_len - WEP_HEADER_LEN)
        .map_err(|_| WepError::InvalidFrame)?;
    clear.extend_from_slice(&frame[header_len + WEP_HEADER_LEN..]);
    rc4.apply(&mut clear);
    if clear.len() < WEP_CRC_LEN {
        return Err(WepError::InvalidFrame);
    }
    let payload_len = clear.len() - WEP_CRC_LEN;
    let received_crc = u32::from_le_bytes(clear[payload_len..].try_into().unwrap());
    let expected_crc = !crc32_le_update(!0, &clear[..payload_len]);
    if received_crc != expected_crc {
        return Err(WepError::IcvMismatch);
    }
    let mut output = alloc::vec::Vec::new();
    output
        .try_reserve_exact(header_len + payload_len)
        .map_err(|_| WepError::InvalidFrame)?;
    output.extend_from_slice(&frame[..header_len]);
    output[1] &= !FC1_PROTECTED;
    output.extend_from_slice(&clear[..payload_len]);
    Ok(output)
}

fn crc32_le_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { CRC32_LE_POLY } else { 0 };
        }
    }
    crc
}

struct Rc4 {
    state: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    fn new(key: &[u8]) -> Self {
        let mut state = [0u8; 256];
        for (index, byte) in state.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let mut j = 0u8;
        for i in 0..256usize {
            j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
            state.swap(i, usize::from(j));
        }
        Self { state, i: 0, j: 0 }
    }

    fn apply(&mut self, data: &mut [u8]) {
        for byte in data {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.state[usize::from(self.i)]);
            self.state.swap(usize::from(self.i), usize::from(self.j));
            let index =
                self.state[usize::from(self.i)].wrapping_add(self.state[usize::from(self.j)]);
            *byte ^= self.state[usize::from(index)];
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn rc4_stream_matches_rfc6229_short_vector() {
        let mut rc4 = Rc4::new(b"Key");
        let mut data = *b"Plaintext";
        rc4.apply(&mut data);
        assert_eq!(data, [0xbb, 0xf3, 0x16, 0xe8, 0xd9, 0x40, 0xaf, 0x0a, 0xd3]);
    }

    #[test]
    fn wep_frame_roundtrip_checks_icv_and_weak_iv_advance() {
        let mut tx = WepKey::set_key(&[0x42; 5], 1).unwrap();
        let rx = WepKey::set_key(&[0x42; 5], 1).unwrap();
        let mut frame = vec![0u8; IEEE80211_HEADER_LEN];
        frame[0] = 0x08;
        frame[1] = 0;
        frame[4..10].copy_from_slice(&[0x02, 0, 0, 0, 0, 1]);
        frame.extend_from_slice(b"WEP payload");
        let encrypted = encrypt_wep(&frame, IEEE80211_HEADER_LEN, &mut tx, 0x03ff00).unwrap();
        // The source avoids this weak-IV range before writing the IV.
        assert_eq!(
            &encrypted[IEEE80211_HEADER_LEN..IEEE80211_HEADER_LEN + 3],
            &[0, 0, 0x04]
        );
        let clear = decrypt_wep(&encrypted, IEEE80211_HEADER_LEN, &rx).unwrap();
        assert_eq!(clear, frame);
        let mut damaged = encrypted;
        *damaged.last_mut().unwrap() ^= 1;
        assert_eq!(
            decrypt_wep(&damaged, IEEE80211_HEADER_LEN, &rx),
            Err(WepError::IcvMismatch)
        );
    }
}
