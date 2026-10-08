//! Hardware-decryption replay bookkeeping from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_input.c` rev 1.263,
//! `ieee80211_crypto_ccmp.c` rev 1.22 and `ieee80211_crypto_tkip.c` rev 1.34
//! (ISC-style). Copyright (c) 2001 Atsushi Onoe; Copyright (c) 2002, 2003
//! Sam Leffler, Errno Consulting; Copyright (c) 2007-2009 Damien Bergamini.

use alloc::vec::Vec;

use crate::{Cipher, HeaderError, has_qos_control, header_length, qos_control};

const FC1_PROTECTED: u8 = 0x40;
const EXT_IV: u8 = 0x20;
const CCMP_HEADER_LEN: usize = 8;
const TKIP_HEADER_LEN: usize = 8;
const QOS_TID_MASK: u16 = 0x000f;
const TID_COUNT: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HardwareReplayState {
    pub data_rsc: [u64; TID_COUNT],
    pub management_rsc: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardwareDecryptError {
    KeyUnavailable,
    Header(HeaderError),
    MissingIv,
    MissingExtendedIv,
    Replay { packet_number: u64, last_seen: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HardwareDecryptResult {
    pub decrypted: bool,
    pub packet_number: Option<u64>,
    pub iv_removed: bool,
}

/// Validate CCMP ExtIV and extract the 48-bit PN in source octet order.
// upstream: ieee80211_crypto_ccmp.c ieee80211_ccmp_get_pn()
fn ccmp_packet_number(iv: &[u8]) -> Result<u64, HardwareDecryptError> {
    if iv.len() < CCMP_HEADER_LEN {
        return Err(HardwareDecryptError::MissingIv);
    }
    if iv[3] & EXT_IV == 0 {
        return Err(HardwareDecryptError::MissingExtendedIv);
    }
    Ok(u64::from(iv[0])
        | (u64::from(iv[1]) << 8)
        | (u64::from(iv[4]) << 16)
        | (u64::from(iv[5]) << 24)
        | (u64::from(iv[6]) << 32)
        | (u64::from(iv[7]) << 40))
}

/// Validate TKIP ExtIV and extract its 48-bit TSC in source octet order.
// upstream: ieee80211_crypto_tkip.c ieee80211_tkip_get_tsc()
fn tkip_packet_number(iv: &[u8]) -> Result<u64, HardwareDecryptError> {
    if iv.len() < TKIP_HEADER_LEN {
        return Err(HardwareDecryptError::MissingIv);
    }
    if iv[3] & EXT_IV == 0 {
        return Err(HardwareDecryptError::MissingExtendedIv);
    }
    Ok(u64::from(iv[2])
        | (u64::from(iv[0]) << 8)
        | (u64::from(iv[4]) << 16)
        | (u64::from(iv[5]) << 24)
        | (u64::from(iv[6]) << 32)
        | (u64::from(iv[7]) << 40))
}

/// Update the post-reorder CCMP/TKIP receive counter, unprotect frame and strip the IV.
// upstream: ieee80211_input.c ieee80211_input_hwdecrypt()
pub fn postprocess_hardware_decryption(
    frame: &mut Vec<u8>,
    cipher: Cipher,
    key_available: bool,
    same_packet_number: bool,
    replay: &mut HardwareReplayState,
) -> Result<HardwareDecryptResult, HardwareDecryptError> {
    if !key_available {
        return Err(HardwareDecryptError::KeyUnavailable);
    }
    let hdrlen = header_length(frame).map_err(HardwareDecryptError::Header)?;
    if frame[1] & FC1_PROTECTED == 0 {
        // Hardware already removed the IV; net80211 trusts its replay filter.
        return Ok(HardwareDecryptResult {
            decrypted: true,
            packet_number: None,
            iv_removed: false,
        });
    }
    let iv_len = match cipher {
        Cipher::Ccmp => CCMP_HEADER_LEN,
        Cipher::Tkip => TKIP_HEADER_LEN,
        _ => {
            return Ok(HardwareDecryptResult {
                decrypted: false,
                packet_number: None,
                iv_removed: false,
            });
        }
    };
    let iv = frame
        .get(hdrlen..hdrlen + iv_len)
        .ok_or(HardwareDecryptError::MissingIv)?;
    let packet_number = if cipher == Cipher::Ccmp {
        ccmp_packet_number(iv)?
    } else {
        tkip_packet_number(iv)?
    };
    let data_frame = frame[0] & 0x0c == 0x08;
    let counter = if cipher == Cipher::Tkip || data_frame {
        let tid = if has_qos_control(frame[0]) {
            usize::from(qos_control(frame).map_err(HardwareDecryptError::Header)? & QOS_TID_MASK)
        } else {
            0
        };
        &mut replay.data_rsc[tid]
    } else {
        &mut replay.management_rsc
    };
    let invalid = if same_packet_number {
        packet_number < *counter
    } else {
        packet_number <= *counter
    };
    if invalid {
        return Err(HardwareDecryptError::Replay {
            packet_number,
            last_seen: *counter,
        });
    }
    *counter = packet_number;
    frame[1] &= !FC1_PROTECTED;
    frame.drain(hdrlen..hdrlen + iv_len);
    Ok(HardwareDecryptResult {
        decrypted: true,
        packet_number: Some(packet_number),
        iv_removed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protected_data_frame(pn: u64, qos_tid: Option<u16>, cipher: Cipher) -> Vec<u8> {
        let qos_len = usize::from(qos_tid.is_some()) * 2;
        let hdrlen = 24 + qos_len;
        let mut frame = vec![0; hdrlen + 8 + 4];
        frame[0] = 0x08 | if qos_tid.is_some() { 0x80 } else { 0 };
        frame[1] = FC1_PROTECTED;
        if let Some(tid) = qos_tid {
            frame[24..26].copy_from_slice(&tid.to_le_bytes());
        }
        let iv = &mut frame[hdrlen..hdrlen + 8];
        if cipher == Cipher::Ccmp {
            iv[0] = pn as u8;
            iv[1] = (pn >> 8) as u8;
            iv[3] = EXT_IV;
            iv[4] = (pn >> 16) as u8;
            iv[5] = (pn >> 24) as u8;
            iv[6] = (pn >> 32) as u8;
            iv[7] = (pn >> 40) as u8;
        } else {
            iv[0] = (pn >> 8) as u8;
            iv[2] = pn as u8;
            iv[3] = EXT_IV;
            iv[4] = (pn >> 16) as u8;
            iv[5] = (pn >> 24) as u8;
            iv[6] = (pn >> 32) as u8;
            iv[7] = (pn >> 40) as u8;
        }
        frame
    }

    #[test]
    fn ccmp_and_tkip_update_per_tid_rsc_then_strip_iv() {
        let mut replay = HardwareReplayState::default();
        let mut ccmp = protected_data_frame(0x0102_0304_0506, Some(3), Cipher::Ccmp);
        let original = ccmp.len();
        let result =
            postprocess_hardware_decryption(&mut ccmp, Cipher::Ccmp, true, false, &mut replay)
                .unwrap();
        assert_eq!(result.packet_number, Some(0x0102_0304_0506));
        assert_eq!(ccmp.len(), original - CCMP_HEADER_LEN);
        assert_eq!(ccmp[1] & FC1_PROTECTED, 0);
        assert_eq!(replay.data_rsc[3], 0x0102_0304_0506);
        let mut tkip = protected_data_frame(0x0102_0304_0507, None, Cipher::Tkip);
        assert!(
            postprocess_hardware_decryption(&mut tkip, Cipher::Tkip, true, false, &mut replay)
                .unwrap()
                .iv_removed
        );
        assert_eq!(replay.data_rsc[0], 0x0102_0304_0507);
    }

    #[test]
    fn replay_same_pn_policy_matches_reorder_path_and_unprotected_skip() {
        let mut replay = HardwareReplayState::default();
        replay.data_rsc[0] = 9;
        let mut replayed = protected_data_frame(9, None, Cipher::Ccmp);
        assert_eq!(
            postprocess_hardware_decryption(&mut replayed, Cipher::Ccmp, true, false, &mut replay),
            Err(HardwareDecryptError::Replay {
                packet_number: 9,
                last_seen: 9
            })
        );
        let mut amsdu = protected_data_frame(9, None, Cipher::Ccmp);
        assert!(
            postprocess_hardware_decryption(&mut amsdu, Cipher::Ccmp, true, true, &mut replay)
                .unwrap()
                .iv_removed
        );
        let mut already_stripped = vec![0; 24];
        already_stripped[0] = 0x08;
        let result = postprocess_hardware_decryption(
            &mut already_stripped,
            Cipher::Ccmp,
            true,
            false,
            &mut replay,
        )
        .unwrap();
        assert!(result.decrypted);
        assert!(!result.iv_removed);
        assert_eq!(already_stripped.len(), 24);
        assert_eq!(
            postprocess_hardware_decryption(
                &mut already_stripped,
                Cipher::Ccmp,
                false,
                false,
                &mut replay
            ),
            Err(HardwareDecryptError::KeyUnavailable)
        );
    }
}
