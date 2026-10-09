//! Software CCMP crypto from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_crypto_ccmp.c` rev 1.22 (ISC) and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2008 Damien
//! Bergamini <damien.bergamini@free.fr>.

use aes::{
    Aes128, Block,
    cipher::{BlockEncrypt, KeyInit},
};

const IEEE80211_HEADER_LEN: usize = 24;
const CCMP_HEADER_LEN: usize = 8;
const CCMP_MIC_LEN: usize = 8;
const CCMP_EXT_IV: u8 = 0x20;
const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_MGMT: u8 = 0x00;
const FC0_TYPE_DATA: u8 = 0x08;
const FC0_SUBTYPE_MASK: u8 = 0xf0;
const FC0_SUBTYPE_QOS: u8 = 0x80;
const FC1_TO_DS: u8 = 0x01;
const FC1_FROM_DS: u8 = 0x02;
const FC1_RETRY: u8 = 0x08;
const FC1_PWR_MGT: u8 = 0x10;
const FC1_MORE_DATA: u8 = 0x20;
const FC1_PROTECTED: u8 = 0x40;
const FC1_ORDER: u8 = 0x80;
const SEQUENCE_MASK: u8 = 0xf0;
const QOS_TID_MASK: u8 = 0x0f;
const PN_MAX: u64 = (1 << 48) - 1;
const TID_COUNT: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CcmpError {
    InvalidKeyLength,
    InvalidKeyId,
    InvalidHeader,
    InvalidFrame,
    MissingExtendedIv,
    PacketNumberExhausted,
    Replay { packet_number: u64, last_seen: u64 },
    MicMismatch,
}

/// Per-key software CCMP context with source TX/RX packet-number state.
pub struct CcmpKey {
    key: [u8; 16],
    pub key_id: u8,
    pub tx_packet_number: u64,
    pub data_replay_counters: [u64; TID_COUNT],
    pub management_replay_counter: u64,
}

impl CcmpKey {
    /// Initialize the AES context from a 128-bit CCMP key.
    // upstream: ieee80211_crypto_ccmp.c ieee80211_ccmp_set_key()
    pub fn set_key(key: &[u8], key_id: u8) -> Result<Self, CcmpError> {
        if key.len() != 16 {
            return Err(CcmpError::InvalidKeyLength);
        }
        if key_id > 3 {
            return Err(CcmpError::InvalidKeyId);
        }
        let mut material = [0; 16];
        material.copy_from_slice(key);
        Ok(Self {
            key: material,
            key_id,
            tx_packet_number: 0,
            data_replay_counters: [0; TID_COUNT],
            management_replay_counter: 0,
        })
    }

    /// Clear the software key material and replay counters before releasing it.
    // upstream: ieee80211_crypto_ccmp.c ieee80211_ccmp_delete_key()
    pub fn delete_key(&mut self) {
        for byte in &mut self.key {
            // SAFETY: `byte` is a valid, uniquely borrowed byte of key storage.
            unsafe { core::ptr::write_volatile(byte, 0) };
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        self.key_id = 0;
        self.tx_packet_number = 0;
        self.data_replay_counters.fill(0);
        self.management_replay_counter = 0;
    }
}

impl Drop for CcmpKey {
    fn drop(&mut self) {
        self.delete_key();
    }
}

/// Construct the source AAD and CCM counter state for one 802.11 frame.
// upstream: ieee80211_crypto_ccmp.c ieee80211_ccmp_phase1()
fn phase1(
    cipher: &Aes128,
    frame: &[u8],
    header_len: usize,
    packet_number: u64,
    message_len: usize,
) -> Result<([u8; 16], [u8; 16], [u8; 16]), CcmpError> {
    if header_len < IEEE80211_HEADER_LEN
        || frame.len() < header_len
        || message_len > u16::MAX as usize
    {
        return Err(CcmpError::InvalidHeader);
    }
    let fc0 = frame[0];
    let fc1 = frame[1];
    let qos = is_qos_data(fc0);
    let addr4 = fc1 & (FC1_TO_DS | FC1_FROM_DS) == (FC1_TO_DS | FC1_FROM_DS);
    let qos_offset = IEEE80211_HEADER_LEN + if addr4 { 6 } else { 0 };
    if qos && header_len < qos_offset + 2 {
        return Err(CcmpError::InvalidHeader);
    }

    let mut aad = [0u8; 30];
    let mut aad_len = 0usize;
    let mut frame_control0 = fc0;
    if fc0 & FC0_TYPE_MASK == FC0_TYPE_DATA {
        frame_control0 = (frame_control0 & !FC0_SUBTYPE_MASK) | FC0_SUBTYPE_QOS;
    }
    aad[aad_len] = frame_control0;
    aad_len += 1;
    aad[aad_len] = fc1 & !(FC1_RETRY | FC1_PWR_MGT | FC1_MORE_DATA);
    if qos {
        aad[aad_len] &= !FC1_ORDER;
    }
    aad_len += 1;
    aad[aad_len..aad_len + 18].copy_from_slice(&frame[4..22]);
    aad[aad_len + 18] = frame[22] & !SEQUENCE_MASK;
    aad[aad_len + 19] = 0;
    aad_len += 20;
    if addr4 {
        aad[aad_len..aad_len + 6].copy_from_slice(&frame[24..30]);
        aad_len += 6;
    }
    let tid = if qos {
        frame[qos_offset] & QOS_TID_MASK
    } else {
        0
    };
    if qos {
        aad[aad_len] = tid;
        aad[aad_len + 1] = 0;
        aad_len += 2;
    }

    let mut nonce = [0u8; 13];
    nonce[0] = tid
        | if fc0 & FC0_TYPE_MASK == FC0_TYPE_MGMT {
            1 << 4
        } else {
            0
        };
    nonce[1..7].copy_from_slice(&frame[10..16]);
    for i in 0..6 {
        nonce[7 + i] = (packet_number >> (40 - i * 8)) as u8;
    }
    ccm_state(cipher, &nonce, &aad[..aad_len], message_len)
}

/// Initialize CCM CBC-MAC, S0, and counter state for nonce/AAD/plaintext.
fn ccm_state(
    cipher: &Aes128,
    nonce: &[u8; 13],
    aad: &[u8],
    message_len: usize,
) -> Result<([u8; 16], [u8; 16], [u8; 16]), CcmpError> {
    if message_len > u16::MAX as usize || aad.len() > u16::MAX as usize {
        return Err(CcmpError::InvalidFrame);
    }
    let mut b = [0u8; 16];
    b[0] = if aad.is_empty() { 0x19 } else { 0x59 };
    b[1..14].copy_from_slice(nonce);
    b[14..16].copy_from_slice(&(message_len as u16).to_be_bytes());
    encrypt_block(cipher, &mut b);
    if !aad.is_empty() {
        let encoded_len = 2 + aad.len();
        let padded_len = encoded_len.div_ceil(16) * 16;
        let mut encoded_aad = alloc::vec![0u8; padded_len];
        encoded_aad[..2].copy_from_slice(&(aad.len() as u16).to_be_bytes());
        encoded_aad[2..2 + aad.len()].copy_from_slice(aad);
        for block in encoded_aad.chunks_exact(16) {
            for i in 0..16 {
                b[i] ^= block[i];
            }
            encrypt_block(cipher, &mut b);
        }
    }
    let mut s0_block = [0u8; 16];
    s0_block[0] = 1;
    s0_block[1..14].copy_from_slice(nonce);
    encrypt_block(cipher, &mut s0_block);
    let mut counter = [0u8; 16];
    counter[0] = 1;
    counter[1..14].copy_from_slice(nonce);
    Ok((s0_block, b, counter))
}

fn encrypt_message(
    cipher: &Aes128,
    mut mac: [u8; 16],
    s0: [u8; 16],
    mut counter: [u8; 16],
    message: &[u8],
) -> (alloc::vec::Vec<u8>, [u8; CCMP_MIC_LEN]) {
    let mut encrypted = alloc::vec![0u8; message.len()];
    for (block_index, chunk) in message.chunks(16).enumerate() {
        counter[14..16].copy_from_slice(&((block_index + 1) as u16).to_be_bytes());
        let mut stream = counter;
        encrypt_block(cipher, &mut stream);
        for (offset, byte) in chunk.iter().enumerate() {
            mac[offset] ^= *byte;
            encrypted[block_index * 16 + offset] = *byte ^ stream[offset];
        }
        encrypt_block(cipher, &mut mac);
    }
    let mut tag = [0; CCMP_MIC_LEN];
    for i in 0..CCMP_MIC_LEN {
        tag[i] = mac[i] ^ s0[i];
    }
    (encrypted, tag)
}

fn decrypt_message(
    cipher: &Aes128,
    mut mac: [u8; 16],
    s0: [u8; 16],
    mut counter: [u8; 16],
    ciphertext: &[u8],
) -> (alloc::vec::Vec<u8>, [u8; CCMP_MIC_LEN]) {
    let mut plaintext = alloc::vec![0u8; ciphertext.len()];
    for (block_index, chunk) in ciphertext.chunks(16).enumerate() {
        counter[14..16].copy_from_slice(&((block_index + 1) as u16).to_be_bytes());
        let mut stream = counter;
        encrypt_block(cipher, &mut stream);
        for (offset, byte) in chunk.iter().enumerate() {
            let clear = *byte ^ stream[offset];
            plaintext[block_index * 16 + offset] = clear;
            mac[offset] ^= clear;
        }
        encrypt_block(cipher, &mut mac);
    }
    let mut tag = [0; CCMP_MIC_LEN];
    for i in 0..CCMP_MIC_LEN {
        tag[i] = mac[i] ^ s0[i];
    }
    (plaintext, tag)
}

/// Encrypt an 802.11 frame body and append its ExtIV and CCM MIC.
// upstream: ieee80211_crypto_ccmp.c ieee80211_ccmp_encrypt()
pub fn encrypt_ccmp(
    frame: &[u8],
    header_len: usize,
    key: &mut CcmpKey,
) -> Result<alloc::vec::Vec<u8>, CcmpError> {
    if header_len < IEEE80211_HEADER_LEN || frame.len() < header_len {
        return Err(CcmpError::InvalidHeader);
    }
    if key.tx_packet_number >= PN_MAX {
        return Err(CcmpError::PacketNumberExhausted);
    }
    let packet_number = key.tx_packet_number + 1;
    let message = &frame[header_len..];
    let cipher = Aes128::new_from_slice(&key.key).map_err(|_| CcmpError::InvalidKeyLength)?;
    let mut protected_header = alloc::vec::Vec::new();
    protected_header.extend_from_slice(&frame[..header_len]);
    protected_header[1] |= FC1_PROTECTED;
    let (s0, mac, ctr_block) = phase1(
        &cipher,
        &protected_header,
        header_len,
        packet_number,
        message.len(),
    )?;
    let (encrypted, mic) = encrypt_message(&cipher, mac, s0, ctr_block, message);
    let mut output = alloc::vec::Vec::new();
    let output_size = frame
        .len()
        .checked_add(CCMP_HEADER_LEN + CCMP_MIC_LEN)
        .ok_or(CcmpError::InvalidFrame)?;
    output
        .try_reserve_exact(output_size)
        .map_err(|_| CcmpError::InvalidFrame)?;
    output.extend_from_slice(&frame[..header_len]);
    output[1] |= FC1_PROTECTED;
    output.extend_from_slice(&[
        packet_number as u8,
        (packet_number >> 8) as u8,
        0,
        (key.key_id << 6) | CCMP_EXT_IV,
        (packet_number >> 16) as u8,
        (packet_number >> 24) as u8,
        (packet_number >> 32) as u8,
        (packet_number >> 40) as u8,
    ]);
    output.extend_from_slice(&encrypted);
    output.extend_from_slice(&mic);
    key.tx_packet_number = packet_number;
    Ok(output)
}

/// Parse a CCMP ExtIV and select the data TID or management replay counter.
// upstream: ieee80211_crypto_ccmp.c ieee80211_ccmp_get_pn()
fn get_pn(frame: &[u8], header_len: usize) -> Result<(u64, usize), CcmpError> {
    let iv = frame
        .get(header_len..header_len + CCMP_HEADER_LEN)
        .ok_or(CcmpError::InvalidFrame)?;
    if iv[3] & CCMP_EXT_IV == 0 {
        return Err(CcmpError::MissingExtendedIv);
    }
    let packet_number = u64::from(iv[0])
        | (u64::from(iv[1]) << 8)
        | (u64::from(iv[4]) << 16)
        | (u64::from(iv[5]) << 24)
        | (u64::from(iv[6]) << 32)
        | (u64::from(iv[7]) << 40);
    let tid = if is_qos_data(frame[0]) {
        usize::from(frame[qos_offset(frame, header_len)?] & QOS_TID_MASK)
    } else {
        0
    };
    Ok((packet_number, tid))
}

/// Verify the MIC, reject replayed packets, remove ExtIV/MIC and clear Protected.
// upstream: ieee80211_crypto_ccmp.c ieee80211_ccmp_decrypt()
pub fn decrypt_ccmp(
    frame: &[u8],
    header_len: usize,
    key: &mut CcmpKey,
) -> Result<alloc::vec::Vec<u8>, CcmpError> {
    if header_len < IEEE80211_HEADER_LEN
        || frame.len() < header_len + CCMP_HEADER_LEN + CCMP_MIC_LEN
    {
        return Err(CcmpError::InvalidFrame);
    }
    let (packet_number, tid) = get_pn(frame, header_len)?;
    let management = frame[0] & FC0_TYPE_MASK == FC0_TYPE_MGMT;
    let last_seen = if management {
        key.management_replay_counter
    } else {
        key.data_replay_counters[tid]
    };
    if packet_number <= last_seen {
        return Err(CcmpError::Replay {
            packet_number,
            last_seen,
        });
    }
    let message_start = header_len + CCMP_HEADER_LEN;
    let message_end = frame.len() - CCMP_MIC_LEN;
    let ciphertext = &frame[message_start..message_end];
    let received_mic = &frame[message_end..];
    let cipher = Aes128::new_from_slice(&key.key).map_err(|_| CcmpError::InvalidKeyLength)?;
    let (s0, mac, ctr_block) = phase1(&cipher, frame, header_len, packet_number, ciphertext.len())?;
    let (plaintext, mic) = decrypt_message(&cipher, mac, s0, ctr_block, ciphertext);
    let mut mic_difference = 0u8;
    for i in 0..CCMP_MIC_LEN {
        mic_difference |= mic[i] ^ received_mic[i];
    }
    if mic_difference != 0 {
        return Err(CcmpError::MicMismatch);
    }
    let mut output = alloc::vec::Vec::new();
    output
        .try_reserve_exact(header_len + plaintext.len())
        .map_err(|_| CcmpError::InvalidFrame)?;
    output.extend_from_slice(&frame[..header_len]);
    output[1] &= !FC1_PROTECTED;
    output.extend_from_slice(&plaintext);
    if management {
        key.management_replay_counter = packet_number;
    } else {
        key.data_replay_counters[tid] = packet_number;
    }
    Ok(output)
}

fn is_qos_data(fc0: u8) -> bool {
    fc0 & FC0_TYPE_MASK == FC0_TYPE_DATA && fc0 & FC0_SUBTYPE_QOS != 0
}

fn qos_offset(frame: &[u8], header_len: usize) -> Result<usize, CcmpError> {
    let addr4 = frame[1] & (FC1_TO_DS | FC1_FROM_DS) == (FC1_TO_DS | FC1_FROM_DS);
    let offset = IEEE80211_HEADER_LEN + if addr4 { 6 } else { 0 };
    if offset + 2 > header_len {
        return Err(CcmpError::InvalidHeader);
    }
    Ok(offset)
}

fn encrypt_block(cipher: &Aes128, bytes: &mut [u8; 16]) {
    let mut block = Block::default();
    block.copy_from_slice(bytes);
    cipher.encrypt_block(&mut block);
    bytes.copy_from_slice(&block);
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;

    fn protected_frame() -> Vec<u8> {
        let mut frame = vec![0u8; 24];
        frame[0] = FC0_TYPE_DATA;
        frame[1] = FC1_TO_DS;
        frame[4..10].copy_from_slice(&[0x02, 0, 0, 0, 0, 1]);
        frame[10..16].copy_from_slice(&[0x02, 0, 0, 0, 0, 2]);
        frame[16..22].copy_from_slice(&[0x02, 0, 0, 0, 0, 3]);
        frame.extend_from_slice(b"ccmp payload with a partial final block");
        frame
    }

    #[test]
    fn ccmp_frame_roundtrip_and_replay_counter_match_source_layout() {
        let mut tx_key = CcmpKey::set_key(&[0x42; 16], 2).unwrap();
        let mut rx_key = CcmpKey::set_key(&[0x42; 16], 2).unwrap();
        let clear = protected_frame();
        let encrypted = encrypt_ccmp(&clear, IEEE80211_HEADER_LEN, &mut tx_key).unwrap();
        assert_eq!(
            encrypted[24..32],
            [1, 0, 0, CCMP_EXT_IV | (2 << 6), 0, 0, 0, 0]
        );
        assert_eq!(tx_key.tx_packet_number, 1);
        let decoded = decrypt_ccmp(&encrypted, IEEE80211_HEADER_LEN, &mut rx_key).unwrap();
        assert_eq!(decoded, clear);
        assert_eq!(rx_key.data_replay_counters[0], 1);
        assert!(matches!(
            decrypt_ccmp(&encrypted, IEEE80211_HEADER_LEN, &mut rx_key),
            Err(CcmpError::Replay {
                packet_number: 1,
                last_seen: 1
            })
        ));
    }

    #[test]
    fn ccmp_rejects_mic_corruption_and_clears_key_material() {
        let mut key = CcmpKey::set_key(&[0x19; 16], 0).unwrap();
        let mut encrypted =
            encrypt_ccmp(&protected_frame(), IEEE80211_HEADER_LEN, &mut key).unwrap();
        let mut rx = CcmpKey::set_key(&[0x19; 16], 0).unwrap();
        *encrypted.last_mut().unwrap() ^= 1;
        assert_eq!(
            decrypt_ccmp(&encrypted, IEEE80211_HEADER_LEN, &mut rx),
            Err(CcmpError::MicMismatch)
        );
        rx.delete_key();
        assert_eq!(rx.key, [0; 16]);
        assert_eq!(rx.data_replay_counters, [0; TID_COUNT]);
    }

    #[test]
    fn ccm_core_matches_rfc3610_packet_vector_one() {
        let key = [
            0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd,
            0xce, 0xcf,
        ];
        let nonce = [
            0x00, 0x00, 0x00, 0x03, 0x02, 0x01, 0x00, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5,
        ];
        let aad = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        let message = [
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15,
            0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e,
        ];
        let cipher = Aes128::new_from_slice(&key).unwrap();
        let (s0, mac, counter) = ccm_state(&cipher, &nonce, &aad, message.len()).unwrap();
        let (ciphertext, mic) = encrypt_message(&cipher, mac, s0, counter, &message);
        assert_eq!(
            ciphertext,
            [
                0x58, 0x8c, 0x97, 0x9a, 0x61, 0xc6, 0x63, 0xd2, 0xf0, 0x66, 0xd0, 0xc2, 0xc0, 0xf9,
                0x89, 0x80, 0x6d, 0x5f, 0x6b, 0x61, 0xda, 0xc3, 0x84,
            ]
        );
        assert_eq!(mic, [0x17, 0xe8, 0xd1, 0x2c, 0xfd, 0xf9, 0x26, 0xe0]);
    }
}
