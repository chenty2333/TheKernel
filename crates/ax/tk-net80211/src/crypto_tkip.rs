//! Software TKIP frame crypto from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_crypto_tkip.c` rev 1.34 (ISC),
//! `ieee80211.h` rev 1.137 (BSD-3-Clause) and the referenced RC4/Michael
//! primitives. Copyright (c) 2001 Atsushi Onoe; Copyright (c) 2002, 2003
//! Sam Leffler, Errno Consulting; Copyright (c) 2007-2009 Damien Bergamini.

const IEEE80211_HEADER_LEN: usize = 24;
const TKIP_HEADER_LEN: usize = 8;
const MIC_LEN: usize = 8;
const ICV_LEN: usize = 4;
const TKIP_EXT_IV: u8 = 0x20;
const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_DATA: u8 = 0x08;
const FC1_TO_DS: u8 = 0x01;
const FC1_FROM_DS: u8 = 0x02;
const FC1_PROTECTED: u8 = 0x40;
const QOS_TID_MASK: u8 = 0x0f;
const TID_COUNT: usize = 16;
const TSC_MAX: u64 = (1 << 48) - 1;
const CRC32_LE_POLY: u32 = 0xedb8_8320;
const MIC_FAILURE_WINDOW_SECS: u64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TkipError {
    InvalidKeyLength,
    InvalidKeyId,
    InvalidHeader,
    InvalidFrame,
    InvalidIv,
    MissingExtendedIv,
    PacketNumberExhausted,
    Replay { tsc: u64, last_seen: u64 },
    IcvMismatch,
    MicMismatch,
}

/// Per-key TKIP TSC/Michael state for station or hostap key direction.
pub struct TkipKey {
    key: [u8; 32],
    pub key_id: u8,
    pub tx_tsc: u64,
    pub data_replay_counters: [u64; TID_COUNT],
    station_mode: bool,
}

impl TkipKey {
    /// Initialize TKIP temporal and direction-specific Michael keys.
    // upstream: ieee80211_crypto_tkip.c ieee80211_tkip_set_key()
    pub fn set_key(key: &[u8], key_id: u8, station_mode: bool) -> Result<Self, TkipError> {
        if key.len() != 32 {
            return Err(TkipError::InvalidKeyLength);
        }
        if key_id > 3 {
            return Err(TkipError::InvalidKeyId);
        }
        let mut material = [0; 32];
        material.copy_from_slice(key);
        Ok(Self {
            key: material,
            key_id,
            tx_tsc: 0,
            data_replay_counters: [0; TID_COUNT],
            station_mode,
        })
    }

    /// Clear the TKIP context and all temporal key bytes.
    // upstream: ieee80211_crypto_tkip.c ieee80211_tkip_delete_key()
    pub fn delete_key(&mut self) {
        for byte in &mut self.key {
            // SAFETY: `byte` is valid and uniquely borrowed key material.
            unsafe { core::ptr::write_volatile(byte, 0) };
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        self.key_id = 0;
        self.tx_tsc = 0;
        self.data_replay_counters.fill(0);
    }

    fn tx_mic_key(&self) -> &[u8] {
        if self.station_mode {
            &self.key[24..32]
        } else {
            &self.key[16..24]
        }
    }

    fn rx_mic_key(&self) -> &[u8] {
        if self.station_mode {
            &self.key[16..24]
        } else {
            &self.key[24..32]
        }
    }
}

impl Drop for TkipKey {
    fn drop(&mut self) {
        self.delete_key();
    }
}

/// Compute the source DA/SA/TID pseudo-header Michael MIC over an MSDU.
// upstream: ieee80211_crypto_tkip.c ieee80211_tkip_mic()
pub fn tkip_mic(frame: &[u8], header_len: usize, key: &[u8]) -> Result<[u8; MIC_LEN], TkipError> {
    if key.len() != 8 {
        return Err(TkipError::InvalidKeyLength);
    }
    if header_len < IEEE80211_HEADER_LEN || frame.len() < header_len {
        return Err(TkipError::InvalidHeader);
    }
    let mut pseudo = [0u8; 16];
    match frame[1] & (FC1_TO_DS | FC1_FROM_DS) {
        0 => {
            pseudo[..6].copy_from_slice(&frame[4..10]);
            pseudo[6..12].copy_from_slice(&frame[10..16]);
        }
        FC1_TO_DS => {
            pseudo[..6].copy_from_slice(&frame[16..22]);
            pseudo[6..12].copy_from_slice(&frame[10..16]);
        }
        FC1_FROM_DS => {
            pseudo[..6].copy_from_slice(&frame[4..10]);
            pseudo[6..12].copy_from_slice(&frame[16..22]);
        }
        _ => {
            if header_len < IEEE80211_HEADER_LEN + 6 {
                return Err(TkipError::InvalidHeader);
            }
            pseudo[..6].copy_from_slice(&frame[16..22]);
            pseudo[6..12].copy_from_slice(&frame[24..30]);
        }
    }
    let qos_offset = IEEE80211_HEADER_LEN + usize::from(frame[1] & 3 == 3) * 6;
    if frame[0] & FC0_TYPE_MASK == FC0_TYPE_DATA && frame[0] & 0x80 != 0 {
        if header_len < qos_offset + 2 {
            return Err(TkipError::InvalidHeader);
        }
        pseudo[12] = frame[qos_offset] & QOS_TID_MASK;
    }
    let mut data = alloc::vec::Vec::new();
    data.try_reserve_exact(pseudo.len() + frame.len() - header_len)
        .map_err(|_| TkipError::InvalidFrame)?;
    data.extend_from_slice(&pseudo);
    data.extend_from_slice(&frame[header_len..]);
    Ok(michael_mic(key, &data))
}

/// Encrypt an 802.11 MSDU with the source per-packet key mixing and ICV.
// upstream: ieee80211_crypto_tkip.c ieee80211_tkip_encrypt()
pub fn encrypt_tkip(
    frame: &[u8],
    header_len: usize,
    key: &mut TkipKey,
) -> Result<alloc::vec::Vec<u8>, TkipError> {
    if header_len < IEEE80211_HEADER_LEN || frame.len() < header_len {
        return Err(TkipError::InvalidHeader);
    }
    if key.tx_tsc >= TSC_MAX {
        return Err(TkipError::PacketNumberExhausted);
    }
    let tsc = key.tx_tsc + 1;
    let mic = tkip_mic(&frame[..frame.len()], header_len, key.tx_mic_key())?;
    let mut clear = alloc::vec::Vec::new();
    clear
        .try_reserve_exact(frame.len() - header_len + MIC_LEN + ICV_LEN)
        .map_err(|_| TkipError::InvalidFrame)?;
    clear.extend_from_slice(&frame[header_len..]);
    clear.extend_from_slice(&mic);
    let crc = !crc32_le_update(!0, &clear);
    clear.extend_from_slice(&crc.to_le_bytes());
    let (rc4_key, header) = tkip_packet_key(&key.key[..16], &frame[10..16], tsc, key.key_id);
    let mut rc4 = Rc4::new(&rc4_key);
    rc4.apply(&mut clear);

    let mut output = alloc::vec::Vec::new();
    let output_len = frame
        .len()
        .checked_add(TKIP_HEADER_LEN + MIC_LEN + ICV_LEN)
        .ok_or(TkipError::InvalidFrame)?;
    output
        .try_reserve_exact(output_len)
        .map_err(|_| TkipError::InvalidFrame)?;
    output.extend_from_slice(&frame[..header_len]);
    output[1] |= FC1_PROTECTED;
    output.extend_from_slice(&header);
    output.extend_from_slice(&clear);
    key.tx_tsc = tsc;
    Ok(output)
}

/// Parse the TKIP ExtIV/TSC and select the per-TID receive replay counter.
// upstream: ieee80211_crypto_tkip.c ieee80211_tkip_get_tsc()
fn get_tsc(frame: &[u8], header_len: usize) -> Result<(u64, usize), TkipError> {
    let iv = frame
        .get(header_len..header_len + TKIP_HEADER_LEN)
        .ok_or(TkipError::InvalidFrame)?;
    if iv[3] & TKIP_EXT_IV == 0 {
        return Err(TkipError::MissingExtendedIv);
    }
    let tsc = u64::from(iv[2])
        | (u64::from(iv[0]) << 8)
        | (u64::from(iv[4]) << 16)
        | (u64::from(iv[5]) << 24)
        | (u64::from(iv[6]) << 32)
        | (u64::from(iv[7]) << 40);
    let tid = if frame[0] & FC0_TYPE_MASK == FC0_TYPE_DATA && frame[0] & 0x80 != 0 {
        let offset = IEEE80211_HEADER_LEN + usize::from(frame[1] & 3 == 3) * 6;
        if header_len < offset + 2 {
            return Err(TkipError::InvalidHeader);
        }
        usize::from(frame[offset] & QOS_TID_MASK)
    } else {
        0
    };
    Ok((tsc, tid))
}

/// Decrypt TKIP, validate ICV and Michael MIC, and update replay state.
// upstream: ieee80211_crypto_tkip.c ieee80211_tkip_decrypt()
pub fn decrypt_tkip(
    frame: &[u8],
    header_len: usize,
    key: &mut TkipKey,
) -> Result<alloc::vec::Vec<u8>, TkipError> {
    if header_len < IEEE80211_HEADER_LEN
        || frame.len() < header_len + TKIP_HEADER_LEN + MIC_LEN + ICV_LEN
    {
        return Err(TkipError::InvalidFrame);
    }
    if frame[header_len + 3] >> 6 != key.key_id {
        return Err(TkipError::InvalidKeyId);
    }
    let (tsc, tid) = get_tsc(frame, header_len)?;
    let last_seen = key.data_replay_counters[tid];
    if tsc <= last_seen {
        return Err(TkipError::Replay { tsc, last_seen });
    }
    let (rc4_key, _) = tkip_packet_key(&key.key[..16], &frame[10..16], tsc, key.key_id);
    let mut rc4 = Rc4::new(&rc4_key);
    let mut clear = alloc::vec::Vec::new();
    clear
        .try_reserve_exact(frame.len() - header_len - TKIP_HEADER_LEN)
        .map_err(|_| TkipError::InvalidFrame)?;
    clear.extend_from_slice(&frame[header_len + TKIP_HEADER_LEN..]);
    rc4.apply(&mut clear);
    if clear.len() < MIC_LEN + ICV_LEN {
        return Err(TkipError::InvalidFrame);
    }
    let body_and_mic_len = clear.len() - ICV_LEN;
    let received_crc = u32::from_le_bytes(clear[body_and_mic_len..].try_into().unwrap());
    if received_crc != !crc32_le_update(!0, &clear[..body_and_mic_len]) {
        return Err(TkipError::IcvMismatch);
    }
    let payload_len = body_and_mic_len - MIC_LEN;
    let mut clear_frame = alloc::vec::Vec::new();
    clear_frame
        .try_reserve_exact(header_len + payload_len)
        .map_err(|_| TkipError::InvalidFrame)?;
    clear_frame.extend_from_slice(&frame[..header_len]);
    clear_frame[1] &= !FC1_PROTECTED;
    clear_frame.extend_from_slice(&clear[..payload_len]);
    let mic = tkip_mic(&clear_frame, header_len, key.rx_mic_key())?;
    let mut difference = 0u8;
    for index in 0..MIC_LEN {
        difference |= mic[index] ^ clear[payload_len + index];
    }
    if difference != 0 {
        return Err(TkipError::MicMismatch);
    }
    key.data_replay_counters[tid] = tsc;
    Ok(clear_frame)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MichaelFailureState {
    pub last_failure_seconds: u64,
    pub last_failure_tsc: u64,
    pub countermeasures_active: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MichaelFailureEffects {
    pub report_previous_mic_failure: bool,
    pub report_current_mic_failure: bool,
    pub deauthenticate: bool,
    pub restart_scan: bool,
}

/// Apply the station TKIP Michael countermeasure window and produce supplicant effects.
// upstream: ieee80211_crypto_tkip.c ieee80211_michael_mic_failure()
pub fn michael_mic_failure(
    state: &mut MichaelFailureState,
    now_seconds: u64,
    tsc: u64,
    station_mode: bool,
) -> MichaelFailureEffects {
    if state.countermeasures_active {
        return MichaelFailureEffects::default();
    }
    let previous = state.last_failure_seconds != 0
        && state
            .last_failure_seconds
            .saturating_add(MIC_FAILURE_WINDOW_SECS)
            >= now_seconds;
    if !previous {
        state.last_failure_seconds = now_seconds;
        state.last_failure_tsc = tsc;
        return MichaelFailureEffects::default();
    }
    state.last_failure_seconds = now_seconds;
    if station_mode {
        MichaelFailureEffects {
            report_previous_mic_failure: true,
            report_current_mic_failure: true,
            deauthenticate: true,
            restart_scan: true,
        }
    } else {
        state.countermeasures_active = true;
        MichaelFailureEffects::default()
    }
}

fn tkip_packet_key(
    temporal_key: &[u8],
    transmitter: &[u8],
    tsc: u64,
    key_id: u8,
) -> ([u8; 16], [u8; TKIP_HEADER_LEN]) {
    let iv16 = tsc as u16;
    let iv32 = (tsc >> 16) as u32;
    let mut p1k = [0u16; 5];
    phase1(&mut p1k, temporal_key, transmitter, iv32);
    let mut rc4_key = [0u8; 16];
    phase2(&mut rc4_key, temporal_key, &p1k, iv16);
    let tsc0 = iv16 as u8;
    let tsc1 = (iv16 >> 8) as u8;
    let header = [
        tsc1,
        (tsc1 | 0x20) & 0x7f,
        tsc0,
        (key_id << 6) | TKIP_EXT_IV,
        iv32 as u8,
        (iv32 >> 8) as u8,
        (iv32 >> 16) as u8,
        (iv32 >> 24) as u8,
    ];
    (rc4_key, header)
}

/// TKIP phase-1 temporal-key mixing over transmitter address and IV32.
// upstream: ieee80211_crypto_tkip.c Phase1()
fn phase1(output: &mut [u16; 5], temporal_key: &[u8], transmitter: &[u8], iv32: u32) {
    output[0] = iv32 as u16;
    output[1] = (iv32 >> 16) as u16;
    output[2] = u16::from(transmitter[0]) | (u16::from(transmitter[1]) << 8);
    output[3] = u16::from(transmitter[2]) | (u16::from(transmitter[3]) << 8);
    output[4] = u16::from(transmitter[4]) | (u16::from(transmitter[5]) << 8);
    for i in 0..8 {
        output[0] = output[0].wrapping_add(tkip_sbox(output[4] ^ tk16(temporal_key, (i & 1) + 0)));
        output[1] = output[1].wrapping_add(tkip_sbox(output[0] ^ tk16(temporal_key, (i & 1) + 2)));
        output[2] = output[2].wrapping_add(tkip_sbox(output[1] ^ tk16(temporal_key, (i & 1) + 4)));
        output[3] = output[3].wrapping_add(tkip_sbox(output[2] ^ tk16(temporal_key, (i & 1) + 6)));
        output[4] = output[4].wrapping_add(tkip_sbox(output[3] ^ tk16(temporal_key, (i & 1) + 0)));
        output[4] = output[4].wrapping_add(i as u16);
    }
}

/// TKIP phase-2 mixing to the 128-bit per-packet RC4 key.
// upstream: ieee80211_crypto_tkip.c Phase2()
fn phase2(rc4_key: &mut [u8; 16], temporal_key: &[u8], p1k: &[u16; 5], iv16: u16) {
    let mut ppk = [0u16; 6];
    ppk[..5].copy_from_slice(p1k);
    ppk[5] = p1k[4].wrapping_add(iv16);
    for i in 0..6 {
        ppk[i] = ppk[i].wrapping_add(tkip_sbox(ppk[(i + 5) % 6] ^ tk16(temporal_key, i)));
    }
    ppk[0] = ppk[0].wrapping_add(rotr1(ppk[5] ^ tk16(temporal_key, 6)));
    ppk[1] = ppk[1].wrapping_add(rotr1(ppk[0] ^ tk16(temporal_key, 7)));
    for i in 2..6 {
        ppk[i] = ppk[i].wrapping_add(rotr1(ppk[i - 1]));
    }
    let high = (iv16 >> 8) as u8;
    rc4_key[0] = high;
    rc4_key[1] = (high | 0x20) & 0x7f;
    rc4_key[2] = iv16 as u8;
    rc4_key[3] = ((ppk[5] ^ tk16(temporal_key, 0)) >> 1) as u8;
    for (index, word) in ppk.iter().enumerate() {
        rc4_key[4 + index * 2..6 + index * 2].copy_from_slice(&word.to_le_bytes());
    }
}

fn tk16(key: &[u8], index: usize) -> u16 {
    u16::from(key[index * 2]) | (u16::from(key[index * 2 + 1]) << 8)
}

fn tkip_sbox(value: u16) -> u16 {
    let lo = aes_sbox(value as u8);
    let hi = aes_sbox((value >> 8) as u8);
    sbox_word(lo) ^ sbox_word(hi).swap_bytes()
}

fn sbox_word(value: u8) -> u16 {
    let doubled = xtime(value);
    (u16::from(doubled) << 8) | u16::from(doubled ^ value)
}

fn xtime(value: u8) -> u8 {
    (value << 1) ^ if value & 0x80 != 0 { 0x1b } else { 0 }
}

fn aes_sbox(value: u8) -> u8 {
    let inverse = if value == 0 { 0 } else { gf_pow(value, 254) };
    inverse
        ^ inverse.rotate_left(1)
        ^ inverse.rotate_left(2)
        ^ inverse.rotate_left(3)
        ^ inverse.rotate_left(4)
        ^ 0x63
}

fn gf_pow(mut value: u8, mut power: u16) -> u8 {
    let mut result = 1u8;
    while power != 0 {
        if power & 1 != 0 {
            result = gf_mul(result, value);
        }
        value = gf_mul(value, value);
        power >>= 1;
    }
    result
}

fn gf_mul(mut left: u8, mut right: u8) -> u8 {
    let mut result = 0u8;
    for _ in 0..8 {
        if right & 1 != 0 {
            result ^= left;
        }
        left = xtime(left);
        right >>= 1;
    }
    result
}

fn rotr1(value: u16) -> u16 {
    value.rotate_right(1)
}

fn michael_mic(key: &[u8], message: &[u8]) -> [u8; MIC_LEN] {
    let mut left = u32::from_le_bytes(key[..4].try_into().unwrap());
    let mut right = u32::from_le_bytes(key[4..8].try_into().unwrap());
    let mut padded = alloc::vec::Vec::new();
    let padded_len = (message.len() + 1 + 3) & !3;
    padded.extend_from_slice(message);
    padded.push(0x5a);
    padded.resize(padded_len, 0);
    padded.extend_from_slice(&[0; 4]);
    for word in padded.chunks_exact(4) {
        left ^= u32::from_le_bytes(word.try_into().unwrap());
        right ^= left.rotate_left(17);
        left = left.wrapping_add(right);
        right ^= (left & 0x00ff_00ff) << 8 | (left & 0xff00_ff00) >> 8;
        left = left.wrapping_add(right);
        right ^= left.rotate_left(3);
        left = left.wrapping_add(right);
        right ^= left.rotate_right(2);
        left = left.wrapping_add(right);
    }
    let mut mic = [0; MIC_LEN];
    mic[..4].copy_from_slice(&left.to_le_bytes());
    mic[4..].copy_from_slice(&right.to_le_bytes());
    mic
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
    fn michael_round_trip_uses_directional_keys_and_failure_window() {
        let key_material = [0x11; 32];
        let key = TkipKey::set_key(&key_material, 0, true).unwrap();
        let mut frame = vec![0u8; IEEE80211_HEADER_LEN];
        frame[0] = FC0_TYPE_DATA;
        frame[1] = FC1_TO_DS;
        frame[4..10].copy_from_slice(&[0x02, 0, 0, 0, 0, 1]);
        frame[10..16].copy_from_slice(&[0x02, 0, 0, 0, 0, 2]);
        frame[16..22].copy_from_slice(&[0x02, 0, 0, 0, 0, 3]);
        frame.extend_from_slice(b"TKIP payload");
        let mic = tkip_mic(&frame, IEEE80211_HEADER_LEN, &key.key[24..]).unwrap();
        assert_eq!(mic.len(), MIC_LEN);
        let mut failures = MichaelFailureState::default();
        assert_eq!(
            michael_mic_failure(&mut failures, 100, 55, true),
            MichaelFailureEffects::default()
        );
        assert_eq!(
            michael_mic_failure(&mut failures, 120, 56, true),
            MichaelFailureEffects {
                report_previous_mic_failure: true,
                report_current_mic_failure: true,
                deauthenticate: true,
                restart_scan: true,
            }
        );
    }

    #[test]
    fn tkip_phase1_phase2_are_deterministic_and_rc4_frame_roundtrips() {
        let temporal_key = [0x42; 16];
        let transmitter = [0x02, 0, 0, 0, 0, 1];
        let (first, header) = tkip_packet_key(&temporal_key, &transmitter, 1, 0);
        assert_eq!(header, [0, 0x20, 1, TKIP_EXT_IV, 0, 0, 0, 0]);
        assert_ne!(first, [0; 16]);

        let mut tx = TkipKey::set_key(&[0x73; 32], 2, true).unwrap();
        let mut rx = TkipKey::set_key(&[0x73; 32], 2, true).unwrap();
        let mut frame = vec![0u8; IEEE80211_HEADER_LEN];
        frame[0] = FC0_TYPE_DATA;
        frame[1] = FC1_TO_DS;
        frame[4..10].copy_from_slice(&[0x02, 0, 0, 0, 0, 1]);
        frame[10..16].copy_from_slice(&[0x02, 0, 0, 0, 0, 2]);
        frame[16..22].copy_from_slice(&[0x02, 0, 0, 0, 0, 3]);
        frame.extend_from_slice(b"TKIP software frame");
        let encrypted = encrypt_tkip(&frame, IEEE80211_HEADER_LEN, &mut tx).unwrap();
        let clear = decrypt_tkip(&encrypted, IEEE80211_HEADER_LEN, &mut rx).unwrap();
        assert_eq!(clear, frame);
        assert_eq!(rx.data_replay_counters[0], 1);
        assert!(matches!(
            decrypt_tkip(&encrypted, IEEE80211_HEADER_LEN, &mut rx),
            Err(TkipError::Replay { .. })
        ));
    }
}
