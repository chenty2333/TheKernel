//! Software BIP management-frame integrity from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_crypto_bip.c` rev 1.10 (ISC) and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2008 Damien Bergamini
//! <damien.bergamini@free.fr>.

use aes::{
    Aes128, Block,
    cipher::{BlockEncrypt, KeyInit},
};

const IEEE80211_HEADER_LEN: usize = 24;
const MMIE_LEN: usize = 18;
const MMIE_ELEM_ID: u8 = 76;
const MMIE_BODY_LEN: u8 = 16;
const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_MGMT: u8 = 0;
const FC1_RETRY: u8 = 0x08;
const FC1_PWR_MGT: u8 = 0x10;
const FC1_MORE_DATA: u8 = 0x20;
const FC1_PROTECTED: u8 = 0x40;
const CMAC_RB: u8 = 0x87;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BipError {
    InvalidKeyLength,
    InvalidKeyId,
    InvalidFrame,
    InvalidMmIE,
    Replay { packet_number: u64, last_seen: u64 },
    MicMismatch,
}

/// Per-IGTK BIP software context with transmit and management receive counters.
pub struct BipKey {
    key: [u8; 16],
    pub key_id: u16,
    pub tx_packet_number: u64,
    pub management_replay_counter: u64,
}

impl BipKey {
    /// Initialize a software AES-CMAC context for an IGTK.
    // upstream: ieee80211_crypto_bip.c ieee80211_bip_set_key()
    pub fn set_key(key: &[u8], key_id: u16) -> Result<Self, BipError> {
        if key.len() != 16 {
            return Err(BipError::InvalidKeyLength);
        }
        if !(4..=7).contains(&key_id) {
            return Err(BipError::InvalidKeyId);
        }
        let mut material = [0; 16];
        material.copy_from_slice(key);
        Ok(Self {
            key: material,
            key_id,
            tx_packet_number: 0,
            management_replay_counter: 0,
        })
    }

    /// Zeroize key bytes and replay state before releasing an IGTK.
    // upstream: ieee80211_crypto_bip.c ieee80211_bip_delete_key()
    pub fn delete_key(&mut self) {
        for byte in &mut self.key {
            // SAFETY: `byte` is valid and uniquely borrowed key material.
            unsafe { core::ptr::write_volatile(byte, 0) };
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        self.key_id = 0;
        self.tx_packet_number = 0;
        self.management_replay_counter = 0;
    }
}

impl Drop for BipKey {
    fn drop(&mut self) {
        self.delete_key();
    }
}

/// Add a Management MIC IE to a group-addressed management frame.
// upstream: ieee80211_crypto_bip.c ieee80211_bip_encap()
pub fn bip_encap(frame: &[u8], key: &mut BipKey) -> Result<alloc::vec::Vec<u8>, BipError> {
    if frame.len() < IEEE80211_HEADER_LEN || frame[0] & FC0_TYPE_MASK != FC0_TYPE_MGMT {
        return Err(BipError::InvalidFrame);
    }
    if key.tx_packet_number >= (1u64 << 48) - 1 {
        return Err(BipError::InvalidFrame);
    }
    let packet_number = key.tx_packet_number + 1;
    let mut output = alloc::vec::Vec::new();
    let output_len = frame
        .len()
        .checked_add(MMIE_LEN)
        .ok_or(BipError::InvalidFrame)?;
    output
        .try_reserve_exact(output_len)
        .map_err(|_| BipError::InvalidFrame)?;
    output.extend_from_slice(frame);
    output[1] &= !FC1_PROTECTED;
    let mmie_offset = output.len();
    output.extend_from_slice(&[MMIE_ELEM_ID, MMIE_BODY_LEN]);
    output.extend_from_slice(&key.key_id.to_le_bytes());
    put_ipn(&mut output, packet_number);
    output.extend_from_slice(&[0; 8]);
    let mic = bip_mic(&key.key, &output)?;
    output[mmie_offset + 10..mmie_offset + 18].copy_from_slice(&mic[..8]);
    key.tx_packet_number = packet_number;
    Ok(output)
}

/// Verify the MMIE with AES-CMAC, reject replay, and trim the verified suffix.
// upstream: ieee80211_crypto_bip.c ieee80211_bip_decap()
pub fn bip_decap(frame: &[u8], key: &mut BipKey) -> Result<alloc::vec::Vec<u8>, BipError> {
    if frame.len() < IEEE80211_HEADER_LEN + MMIE_LEN || frame[0] & FC0_TYPE_MASK != FC0_TYPE_MGMT {
        return Err(BipError::InvalidFrame);
    }
    let mmie_offset = frame.len() - MMIE_LEN;
    let mmie = &frame[mmie_offset..];
    if mmie[0] != MMIE_ELEM_ID || mmie[1] != MMIE_BODY_LEN {
        return Err(BipError::InvalidMmIE);
    }
    let packet_number = get_ipn(&mmie[4..10]);
    if packet_number <= key.management_replay_counter {
        return Err(BipError::Replay {
            packet_number,
            last_seen: key.management_replay_counter,
        });
    }
    let mut mic_input = alloc::vec::Vec::new();
    mic_input
        .try_reserve_exact(frame.len())
        .map_err(|_| BipError::InvalidFrame)?;
    mic_input.extend_from_slice(frame);
    let mut expected = [0; 8];
    expected.copy_from_slice(&mmie[10..18]);
    mic_input[mmie_offset + 10..mmie_offset + 18].fill(0);
    let actual = bip_mic(&key.key, &mic_input)?;
    let mut difference = 0u8;
    for index in 0..8 {
        difference |= expected[index] ^ actual[index];
    }
    if difference != 0 {
        return Err(BipError::MicMismatch);
    }
    let mut output = alloc::vec::Vec::new();
    output
        .try_reserve_exact(mmie_offset)
        .map_err(|_| BipError::InvalidFrame)?;
    output.extend_from_slice(&frame[..mmie_offset]);
    key.management_replay_counter = packet_number;
    Ok(output)
}

fn bip_mic(key: &[u8; 16], frame: &[u8]) -> Result<[u8; 16], BipError> {
    if frame.len() < IEEE80211_HEADER_LEN {
        return Err(BipError::InvalidFrame);
    }
    let cipher = Aes128::new_from_slice(key).map_err(|_| BipError::InvalidKeyLength)?;
    let mut aad = [0u8; IEEE80211_HEADER_LEN - 4];
    aad[0] = frame[0];
    aad[1] = frame[1] & !(FC1_RETRY | FC1_PWR_MGT | FC1_MORE_DATA);
    aad[2..].copy_from_slice(&frame[4..22]);
    let mut message = alloc::vec::Vec::new();
    message
        .try_reserve_exact(aad.len() + frame.len() - IEEE80211_HEADER_LEN)
        .map_err(|_| BipError::InvalidFrame)?;
    message.extend_from_slice(&aad);
    message.extend_from_slice(&frame[IEEE80211_HEADER_LEN..]);
    Ok(aes_cmac(&cipher, &message))
}

fn aes_cmac(cipher: &Aes128, message: &[u8]) -> [u8; 16] {
    let mut zero = [0u8; 16];
    encrypt_block(cipher, &mut zero);
    let first = derive_subkey(zero);
    let second = derive_subkey(first);
    let complete = !message.is_empty() && message.len().is_multiple_of(16);
    let blocks = message.len().div_ceil(16).max(1);
    let mut state = [0u8; 16];
    for block_index in 0..blocks.saturating_sub(1) {
        for index in 0..16 {
            state[index] ^= message[block_index * 16 + index];
        }
        encrypt_block(cipher, &mut state);
    }
    let final_offset = (blocks - 1) * 16;
    let remaining = message.len().saturating_sub(final_offset);
    let mut final_block = [0u8; 16];
    if complete {
        final_block.copy_from_slice(&message[final_offset..final_offset + 16]);
        for index in 0..16 {
            final_block[index] ^= first[index];
        }
    } else {
        final_block[..remaining].copy_from_slice(&message[final_offset..]);
        final_block[remaining] = 0x80;
        for index in 0..16 {
            final_block[index] ^= second[index];
        }
    }
    for index in 0..16 {
        state[index] ^= final_block[index];
    }
    encrypt_block(cipher, &mut state);
    state
}

fn derive_subkey(mut input: [u8; 16]) -> [u8; 16] {
    let carry = input[0] & 0x80 != 0;
    for index in 0..15 {
        input[index] = (input[index] << 1) | (input[index + 1] >> 7);
    }
    input[15] <<= 1;
    if carry {
        input[15] ^= CMAC_RB;
    }
    input
}

fn put_ipn(output: &mut alloc::vec::Vec<u8>, packet_number: u64) {
    for index in 0..6 {
        output.push((packet_number >> (8 * index)) as u8);
    }
}

fn get_ipn(ipn: &[u8]) -> u64 {
    u64::from(ipn[0])
        | (u64::from(ipn[1]) << 8)
        | (u64::from(ipn[2]) << 16)
        | (u64::from(ipn[3]) << 24)
        | (u64::from(ipn[4]) << 32)
        | (u64::from(ipn[5]) << 40)
}

fn encrypt_block(cipher: &Aes128, bytes: &mut [u8; 16]) {
    let mut block = Block::default();
    block.copy_from_slice(bytes);
    cipher.encrypt_block(&mut block);
    bytes.copy_from_slice(&block);
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn aes_cmac_matches_rfc4493_vectors() {
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let cipher = Aes128::new_from_slice(&key).unwrap();
        assert_eq!(
            aes_cmac(&cipher, &[]),
            [
                0xbb, 0x1d, 0x69, 0x29, 0xe9, 0x59, 0x37, 0x28, 0x7f, 0xa3, 0x7d, 0x12, 0x9b, 0x75,
                0x67, 0x46,
            ]
        );
        let message = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        assert_eq!(
            aes_cmac(&cipher, &message),
            [
                0x07, 0x0a, 0x16, 0xb4, 0x6b, 0x4d, 0x41, 0x44, 0xf7, 0x9b, 0xdd, 0x9d, 0xd0, 0x4a,
                0x28, 0x7c,
            ]
        );
    }

    #[test]
    fn bip_mmie_roundtrip_replay_and_mic_failure_match_source() {
        let mut tx = BipKey::set_key(&[0x52; 16], 4).unwrap();
        let mut rx = BipKey::set_key(&[0x52; 16], 4).unwrap();
        let mut frame = vec![0u8; IEEE80211_HEADER_LEN];
        frame[0] = FC0_TYPE_MGMT | 0xd0;
        frame[1] = FC1_PROTECTED | FC1_RETRY;
        frame[4..10].copy_from_slice(&[0x01, 0, 0, 0, 0, 1]);
        frame[10..16].copy_from_slice(&[0x02, 0, 0, 0, 0, 2]);
        frame[16..22].copy_from_slice(&[0x03, 0, 0, 0, 0, 3]);
        frame.extend_from_slice(b"robust group management");
        let protected = bip_encap(&frame, &mut tx).unwrap();
        assert_eq!(
            &protected[protected.len() - MMIE_LEN..][..4],
            &[76, 16, 4, 0]
        );
        let clear = bip_decap(&protected, &mut rx).unwrap();
        assert_eq!(clear[1] & FC1_PROTECTED, 0);
        let mut expected_header = frame[..IEEE80211_HEADER_LEN].to_vec();
        expected_header[1] &= !FC1_PROTECTED;
        assert_eq!(&clear[..IEEE80211_HEADER_LEN], &expected_header);
        assert_eq!(rx.management_replay_counter, 1);
        assert!(matches!(
            bip_decap(&protected, &mut rx),
            Err(BipError::Replay { .. })
        ));

        let mut corrupted = bip_encap(&frame, &mut tx).unwrap();
        *corrupted.last_mut().unwrap() ^= 1;
        let mut fresh_rx = BipKey::set_key(&[0x52; 16], 4).unwrap();
        assert_eq!(
            bip_decap(&corrupted, &mut fresh_rx),
            Err(BipError::MicMismatch)
        );
    }
}
