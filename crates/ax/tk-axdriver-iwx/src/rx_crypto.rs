//! CCMP replay and hardware-decryption validation from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

pub const RX_MPDU_STATUS_MIC_OK: u32 = 1 << 6;
pub const RX_MPDU_STATUS_CCM_ENCRYPTED: u32 = 2 << 8;
pub const RX_MPDU_STATUS_ENCRYPTION_MASK: u32 = 7 << 8;
pub const RX_MPDU_STATUS_DEC_DONE: u32 = 1 << 11;
pub const CCMP_EXTENDED_IV: u8 = 1 << 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcmpReplayError {
    TruncatedHeader,
    MissingExtendedIv,
    Replay { received: u64, last_seen: u64 },
}

/// CCMP packet number and replay-window predicate for the selected QoS TID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CcmpReplayWindow {
    pub last_seen: u64,
    pub replay_count: u64,
}

impl CcmpReplayWindow {
    pub const fn new() -> Self {
        Self {
            last_seen: 0,
            replay_count: 0,
        }
    }

    /// Check the 48-bit PN; `same_pn` permits equal PNs for later A-MSDU subframes.
    // upstream: if_iwx.c iwx_ccmp_decap()
    pub fn check(&mut self, ccmp_header: &[u8], same_pn: bool) -> Result<u64, CcmpReplayError> {
        if ccmp_header.len() < 8 {
            return Err(CcmpReplayError::TruncatedHeader);
        }
        if ccmp_header[3] & CCMP_EXTENDED_IV == 0 {
            return Err(CcmpReplayError::MissingExtendedIv);
        }
        let packet_number = u64::from(ccmp_header[0])
            | (u64::from(ccmp_header[1]) << 8)
            | (u64::from(ccmp_header[4]) << 16)
            | (u64::from(ccmp_header[5]) << 24)
            | (u64::from(ccmp_header[6]) << 32)
            | (u64::from(ccmp_header[7]) << 40);
        let replay = if same_pn {
            packet_number < self.last_seen
        } else {
            packet_number <= self.last_seen
        };
        if replay {
            self.replay_count = self.replay_count.saturating_add(1);
            return Err(CcmpReplayError::Replay {
                received: packet_number,
                last_seen: self.last_seen,
            });
        }
        Ok(packet_number)
    }
}

impl Default for CcmpReplayWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardwareDecryptPolicy {
    pub receive_protected: bool,
    pub pairwise_ccmp: bool,
    pub group_ccmp: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareDecryptError {
    TruncatedFrame,
    DecryptionFailed,
}

/// Verify RX status before setting the upper-layer hardware-decrypted flag.
// upstream: if_iwx.c iwx_rx_hwdecrypt()
pub fn validate_hardware_decryption(
    frame: &[u8],
    rx_packet_status: u32,
    policy: HardwareDecryptPolicy,
) -> Result<bool, HardwareDecryptError> {
    if frame.len() < 10 {
        return Err(HardwareDecryptError::TruncatedFrame);
    }
    let frame_control = frame[0];
    let frame_type = frame_control & 0x0c;
    let subtype = frame_control & 0xf0;
    if frame_type == 0x04 || (frame_type == 0x08 && subtype & 0x40 != 0) {
        return Ok(false);
    }
    if frame[1] & 0x40 == 0 || !policy.receive_protected {
        return Ok(false);
    }
    let ccmp_key = if frame[4] & 1 != 0 {
        policy.group_ccmp
    } else {
        policy.pairwise_ccmp
    };
    if !ccmp_key {
        return Ok(false);
    }
    if rx_packet_status & RX_MPDU_STATUS_ENCRYPTION_MASK != RX_MPDU_STATUS_CCM_ENCRYPTED
        || rx_packet_status & (RX_MPDU_STATUS_DEC_DONE | RX_MPDU_STATUS_MIC_OK)
            != (RX_MPDU_STATUS_DEC_DONE | RX_MPDU_STATUS_MIC_OK)
    {
        return Err(HardwareDecryptError::DecryptionFailed);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn ccmp_pn_bytes_and_same_pn_amsdu_exception_match_source() {
        let header = [5, 4, 0, CCMP_EXTENDED_IV, 3, 2, 1, 0];
        let packet_number = 0x01_0203_0405;
        let mut window = CcmpReplayWindow {
            last_seen: packet_number,
            replay_count: 0,
        };
        assert_eq!(window.check(&header, true), Ok(packet_number));
        assert_eq!(
            window.check(&header, false),
            Err(CcmpReplayError::Replay {
                received: packet_number,
                last_seen: packet_number
            })
        );
        assert_eq!(window.replay_count, 1);
        assert_eq!(window.last_seen, packet_number);
    }

    #[test]
    fn hardware_decrypt_verification_ignores_control_and_qos_null_frames() {
        let policy = HardwareDecryptPolicy {
            receive_protected: true,
            pairwise_ccmp: true,
            group_ccmp: true,
        };
        let mut control = vec![0; 10];
        control[0] = 0x04;
        let mut qos_null = vec![0; 26];
        qos_null[0] = 0xc8;
        assert_eq!(validate_hardware_decryption(&control, 0, policy), Ok(false));
        assert_eq!(
            validate_hardware_decryption(&qos_null, 0, policy),
            Ok(false)
        );
    }

    #[test]
    fn protected_ccmp_frames_require_both_cipher_and_mic_success_bits() {
        let mut frame = [0; 26];
        frame[0] = 0x08;
        frame[1] = 0x40;
        frame[4] = 0x02; // unicast receiver
        let policy = HardwareDecryptPolicy {
            receive_protected: true,
            pairwise_ccmp: true,
            group_ccmp: false,
        };
        let status = RX_MPDU_STATUS_CCM_ENCRYPTED | RX_MPDU_STATUS_DEC_DONE | RX_MPDU_STATUS_MIC_OK;
        assert_eq!(
            validate_hardware_decryption(&frame, status, policy),
            Ok(true)
        );
        assert_eq!(
            validate_hardware_decryption(&frame, status & !RX_MPDU_STATUS_MIC_OK, policy),
            Err(HardwareDecryptError::DecryptionFailed)
        );
    }
}
