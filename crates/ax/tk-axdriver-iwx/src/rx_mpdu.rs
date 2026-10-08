//! RX_MPDU descriptor validation, frame extraction, and alignment repair.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::DeviceFamily;

const RX_MPDU_STATUS_CRC_OK: u32 = 1 << 0;
const RX_MPDU_STATUS_OVERRUN_OK: u32 = 1 << 1;
pub const RX_MPDU_STATUS_DUPLICATE: u32 = 1 << 22;
const RX_MPDU_STATUS_SEC_ENC_MASK: u32 = 7 << 8;
const RX_MPDU_STATUS_SEC_CCM_ENC: u32 = 2 << 8;
const RX_MPDU_MAC_FLAGS_PAD: u8 = 0x20;
const RX_MPDU_MAC_FLAGS_AMSDU: u8 = 0x40;
const RX_MPDU_AMSDU_SUBFRAME_INDEX: u8 = 0x7f;
const RX_MPDU_AMSDU_LAST_SUBFRAME: u8 = 0x80;
const RX_MPDU_PHY_SHORT_PREAMBLE: u16 = 1 << 7;
const FC_TYPE_MASK: u8 = 0x0c;
const FC_TYPE_CONTROL: u8 = 0x04;
const FC_TYPE_DATA: u8 = 0x08;
const FC_SUBTYPE_MASK: u8 = 0xf0;
const FC_SUBTYPE_CTS: u8 = 0xc0;
const FC_SUBTYPE_ACK: u8 = 0xd0;
const FC_SUBTYPE_QOS: u8 = 0x80;
const FC1_DIRECTION_MASK: u8 = 0x03;
const FC1_ORDER: u8 = 0x80;
const QOS_AMSDU_PRESENT: u16 = 1 << 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxMpduError {
    TruncatedDescriptor,
    BadChecksum,
    FrameTooShort,
    InvalidFrameLength,
    InvalidHeaderLength,
    AllocationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxMpduMetadata {
    pub descriptor_bytes: usize,
    pub frame_bytes: usize,
    pub status: u32,
    pub reorder_data: u32,
    pub phy_info: u16,
    pub mac_flags2: u8,
    pub amsdu_info: u8,
    pub rate_n_flags: u32,
    pub channel_index: u8,
    pub energy_a: u8,
    pub energy_b: u8,
    pub device_timestamp: u32,
}

impl RxMpduMetadata {
    pub const fn short_preamble(self) -> bool {
        self.phy_info & RX_MPDU_PHY_SHORT_PREAMBLE != 0
    }

    pub const fn is_amsdu(self) -> bool {
        self.mac_flags2 & RX_MPDU_MAC_FLAGS_AMSDU != 0
    }

    pub const fn amsdu_subframe(self) -> u8 {
        self.amsdu_info & RX_MPDU_AMSDU_SUBFRAME_INDEX
    }

    pub const fn last_amsdu_subframe(self) -> bool {
        self.amsdu_info & RX_MPDU_AMSDU_LAST_SUBFRAME != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxMpdu<'a> {
    pub metadata: RxMpduMetadata,
    pub frame: &'a [u8],
}

/// Validate checksum/overrun status, family descriptor width, and bounded MPDU length.
// upstream: if_iwx.c iwx_rx_mpdu_mq()
pub fn parse_rx_mpdu(
    payload: &[u8],
    family: DeviceFamily,
    monitor_mode: bool,
) -> Result<RxMpdu<'_>, RxMpduError> {
    let descriptor_bytes = if family >= DeviceFamily::Ax210 {
        68
    } else {
        48
    };
    if payload.len() < descriptor_bytes {
        return Err(RxMpduError::TruncatedDescriptor);
    }
    let frame_bytes = usize::from(read_u16(payload, 0)?);
    let status = read_u32(payload, 12)?;
    if status & (RX_MPDU_STATUS_CRC_OK | RX_MPDU_STATUS_OVERRUN_OK)
        != (RX_MPDU_STATUS_CRC_OK | RX_MPDU_STATUS_OVERRUN_OK)
    {
        return Err(RxMpduError::BadChecksum);
    }
    let minimum = if monitor_mode { 10 } else { 24 };
    if frame_bytes < minimum {
        return Err(RxMpduError::FrameTooShort);
    }
    if frame_bytes > payload.len().saturating_sub(descriptor_bytes) {
        return Err(RxMpduError::InvalidFrameLength);
    }
    let descriptor = &payload[..descriptor_bytes];
    let (rate_offset, energy_offset, channel_offset, timestamp_offset) =
        if family >= DeviceFamily::Ax210 {
            (36, 40, 42, 44)
        } else {
            (28, 32, 34, 36)
        };
    let metadata = RxMpduMetadata {
        descriptor_bytes,
        frame_bytes,
        status,
        reorder_data: read_u32(payload, 16)?,
        phy_info: read_u16(payload, 5)?,
        mac_flags2: payload[3],
        amsdu_info: payload[4],
        rate_n_flags: read_u32(descriptor, rate_offset)?,
        energy_a: descriptor[energy_offset],
        energy_b: descriptor[energy_offset + 1],
        channel_index: descriptor[channel_offset],
        device_timestamp: read_u32(descriptor, timestamp_offset)?,
    };
    Ok(RxMpdu {
        metadata,
        frame: &payload[descriptor_bytes..descriptor_bytes + frame_bytes],
    })
}

/// Remove the hardware's two-byte post-header pad and clear its stale A-MSDU bit.
pub fn normalize_rx_frame(mpdu: RxMpdu<'_>) -> Result<Vec<u8>, RxMpduError> {
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(mpdu.frame.len())
        .map_err(|_| RxMpduError::AllocationFailed)?;
    frame.extend_from_slice(mpdu.frame);
    if mpdu.metadata.mac_flags2 & RX_MPDU_MAC_FLAGS_PAD != 0 {
        let mut header_len = ieee80211_header_len(&frame)?;
        if mpdu.metadata.status & RX_MPDU_STATUS_SEC_ENC_MASK == RX_MPDU_STATUS_SEC_CCM_ENC {
            header_len = header_len
                .checked_add(8)
                .ok_or(RxMpduError::InvalidHeaderLength)?;
        }
        if header_len + 2 > frame.len() {
            return Err(RxMpduError::InvalidHeaderLength);
        }
        frame.drain(header_len..header_len + 2);
    }
    if mpdu.metadata.is_amsdu() {
        clear_qos_amsdu_present(&mut frame);
    }
    Ok(frame)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessedRxMpdu {
    pub metadata: RxMpduMetadata,
    pub frame: Vec<u8>,
    pub hardware_decrypted: bool,
    pub same_sequence: bool,
    pub tid_index: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RxMpduOutcome {
    Deliver(ProcessedRxMpdu),
    DropDuplicate(RxMpduMetadata),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxMpduProcessError {
    Descriptor(RxMpduError),
    HardwareDecrypt(crate::HardwareDecryptError),
    CcmpReplay(crate::CcmpReplayError),
    Duplicate(crate::DuplicateError),
}

/// Apply the source RX_MPDU stages through hardware decrypt, CCMP replay, and duplicate filters.
// upstream: if_iwx.c iwx_rx_mpdu_mq() / iwx_rx_hwdecrypt() / iwx_detect_duplicate()
pub fn process_rx_mpdu(
    payload: &[u8],
    family: DeviceFamily,
    monitor_mode: bool,
    decrypt_policy: crate::HardwareDecryptPolicy,
    replay_windows: &mut [crate::CcmpReplayWindow; 9],
    duplicates: &mut crate::RxDuplicateState,
) -> Result<RxMpduOutcome, RxMpduProcessError> {
    let mpdu =
        parse_rx_mpdu(payload, family, monitor_mode).map_err(RxMpduProcessError::Descriptor)?;
    let metadata = mpdu.metadata;
    let frame = normalize_rx_frame(mpdu).map_err(RxMpduProcessError::Descriptor)?;
    let hardware_decrypted =
        crate::validate_hardware_decryption(&frame, metadata.status, decrypt_policy)
            .map_err(RxMpduProcessError::HardwareDecrypt)?;
    if hardware_decrypted {
        let header_bytes = ieee80211_header_len(&frame).map_err(RxMpduProcessError::Descriptor)?;
        let tid = frame_tid_index(&frame).map_err(RxMpduProcessError::Descriptor)?;
        let ccmp_header =
            frame
                .get(header_bytes..header_bytes + 8)
                .ok_or(RxMpduProcessError::Descriptor(
                    RxMpduError::InvalidHeaderLength,
                ))?;
        replay_windows[tid]
            .check(
                ccmp_header,
                metadata.is_amsdu() && metadata.amsdu_subframe() > 0,
            )
            .map_err(RxMpduProcessError::CcmpReplay)?;
    }
    let duplicate = duplicates
        .check(&frame, metadata.is_amsdu(), metadata.amsdu_subframe())
        .map_err(RxMpduProcessError::Duplicate)?;
    if duplicate.duplicate {
        return Ok(RxMpduOutcome::DropDuplicate(metadata));
    }
    Ok(RxMpduOutcome::Deliver(ProcessedRxMpdu {
        metadata,
        frame,
        hardware_decrypted,
        same_sequence: duplicate.same_sequence,
        tid_index: duplicate.tid_index,
    }))
}

fn frame_tid_index(frame: &[u8]) -> Result<usize, RxMpduError> {
    if frame.len() < 24 {
        return Err(RxMpduError::InvalidHeaderLength);
    }
    let is_qos_data = frame[0] & FC_TYPE_MASK == FC_TYPE_DATA && frame[0] & FC_SUBTYPE_QOS != 0;
    if !is_qos_data {
        return Ok(8);
    }
    let offset = 24 + if frame[1] & 0x03 == 0x03 { 6 } else { 0 };
    let qos = frame
        .get(offset..offset + 2)
        .ok_or(RxMpduError::InvalidHeaderLength)?;
    Ok((u16::from_le_bytes([qos[0], qos[1]]) as usize & 0x0f).min(8))
}

fn ieee80211_header_len(frame: &[u8]) -> Result<usize, RxMpduError> {
    if frame.len() < 2 {
        return Err(RxMpduError::InvalidHeaderLength);
    }
    let frame_type = frame[0] & FC_TYPE_MASK;
    let subtype = frame[0] & FC_SUBTYPE_MASK;
    if frame_type == FC_TYPE_CONTROL {
        return Ok(if matches!(subtype, FC_SUBTYPE_CTS | FC_SUBTYPE_ACK) {
            10
        } else {
            10
        });
    }
    let mut header = 24usize;
    if frame_type == FC_TYPE_DATA && frame[1] & FC1_DIRECTION_MASK == FC1_DIRECTION_MASK {
        header += 6;
    }
    if frame_type == FC_TYPE_DATA && subtype & FC_SUBTYPE_QOS != 0 {
        header += 2;
        if frame[1] & FC1_ORDER != 0 {
            header += 4;
        }
    }
    if header > frame.len() {
        return Err(RxMpduError::InvalidHeaderLength);
    }
    Ok(header)
}

fn clear_qos_amsdu_present(frame: &mut [u8]) {
    if frame.len() < 2 || frame[0] & FC_TYPE_MASK != FC_TYPE_DATA {
        return;
    }
    if frame[0] & FC_SUBTYPE_QOS == 0 {
        return;
    }
    let mut qos_offset = 24;
    if frame[1] & FC1_DIRECTION_MASK == FC1_DIRECTION_MASK {
        qos_offset += 6;
    }
    if let Some(qos) = frame.get_mut(qos_offset..qos_offset + 2) {
        let control = u16::from_le_bytes([qos[0], qos[1]]) & !QOS_AMSDU_PRESENT;
        qos.copy_from_slice(&control.to_le_bytes());
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, RxMpduError> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or(RxMpduError::TruncatedDescriptor)?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, RxMpduError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(RxMpduError::TruncatedDescriptor)?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn descriptor_width_and_generation_fields_follow_gen1_and_gen3_layouts() {
        let mut gen3 = vec![0; 68 + 24];
        gen3[0..2].copy_from_slice(&24u16.to_le_bytes());
        gen3[12..16].copy_from_slice(&3u32.to_le_bytes());
        gen3[36..40].copy_from_slice(&0x1122_3344u32.to_le_bytes());
        gen3[42] = 37;
        gen3[44..48].copy_from_slice(&0x5566_7788u32.to_le_bytes());
        let parsed = parse_rx_mpdu(&gen3, DeviceFamily::Ax210, false).unwrap();
        assert_eq!(parsed.metadata.descriptor_bytes, 68);
        assert_eq!(parsed.metadata.rate_n_flags, 0x1122_3344);
        assert_eq!(parsed.metadata.channel_index, 37);
        assert_eq!(parsed.metadata.device_timestamp, 0x5566_7788);

        let mut gen2 = vec![0; 48 + 24];
        gen2[0..2].copy_from_slice(&24u16.to_le_bytes());
        gen2[12..16].copy_from_slice(&3u32.to_le_bytes());
        gen2[28..32].copy_from_slice(&0xaabb_ccddu32.to_le_bytes());
        gen2[34] = 11;
        let parsed = parse_rx_mpdu(&gen2, DeviceFamily::Family22000, false).unwrap();
        assert_eq!(parsed.metadata.descriptor_bytes, 48);
        assert_eq!(parsed.metadata.rate_n_flags, 0xaabb_ccdd);
        assert_eq!(parsed.metadata.channel_index, 11);
    }

    #[test]
    fn descriptor_checks_crc_overrun_minimum_and_bounded_mpdu_length() {
        let mut bytes = vec![0; 68 + 24];
        bytes[..2].copy_from_slice(&24u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            parse_rx_mpdu(&bytes, DeviceFamily::Ax210, false),
            Err(RxMpduError::BadChecksum)
        );
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            parse_rx_mpdu(&bytes, DeviceFamily::Ax210, false),
            Err(RxMpduError::BadChecksum)
        );
        bytes[12..16].copy_from_slice(&3u32.to_le_bytes());
        bytes[..2].copy_from_slice(&25u16.to_le_bytes());
        assert_eq!(
            parse_rx_mpdu(&bytes, DeviceFamily::Ax210, false),
            Err(RxMpduError::InvalidFrameLength)
        );
    }

    #[test]
    fn post_header_pad_is_removed_after_ccmp_iv_and_amsdu_bit_is_cleared() {
        let mut bytes = vec![0; 68 + 40];
        bytes[..2].copy_from_slice(&40u16.to_le_bytes());
        bytes[3] = RX_MPDU_MAC_FLAGS_PAD | RX_MPDU_MAC_FLAGS_AMSDU;
        bytes[12..16].copy_from_slice(&(3u32 | RX_MPDU_STATUS_SEC_CCM_ENC).to_le_bytes());
        let frame = &mut bytes[68..];
        frame[0] = FC_TYPE_DATA | FC_SUBTYPE_QOS;
        frame[24..26].copy_from_slice(&QOS_AMSDU_PRESENT.to_le_bytes());
        frame[34..36].copy_from_slice(&[0xaa, 0xbb]);
        frame[36..40].copy_from_slice(&[1, 2, 3, 4]);
        let mpdu = parse_rx_mpdu(&bytes, DeviceFamily::Ax210, false).unwrap();
        let normalized = normalize_rx_frame(mpdu).unwrap();
        assert_eq!(normalized.len(), 38);
        assert_eq!(u16::from_le_bytes([normalized[24], normalized[25]]), 0);
        assert_eq!(normalized[34..36], [1, 2]);
    }

    #[test]
    fn complete_rx_stage_checks_hw_ccmp_pn_then_duplicate_window() {
        let mut payload = vec![0; 68 + 40];
        payload[..2].copy_from_slice(&40u16.to_le_bytes());
        payload[3] = RX_MPDU_MAC_FLAGS_PAD | RX_MPDU_MAC_FLAGS_AMSDU;
        payload[4] = 1;
        let status = RX_MPDU_STATUS_CRC_OK
            | RX_MPDU_STATUS_OVERRUN_OK
            | RX_MPDU_STATUS_SEC_CCM_ENC
            | crate::RX_MPDU_STATUS_DEC_DONE
            | crate::RX_MPDU_STATUS_MIC_OK;
        payload[12..16].copy_from_slice(&status.to_le_bytes());
        let frame = &mut payload[68..];
        frame[0] = FC_TYPE_DATA | FC_SUBTYPE_QOS;
        frame[1] = 0x40;
        frame[22..24].copy_from_slice(&(12u16 << 4).to_le_bytes());
        frame[24..26].copy_from_slice(&(QOS_AMSDU_PRESENT | 3).to_le_bytes());
        frame[26] = 5;
        frame[29] = crate::CCMP_EXTENDED_IV;
        frame[34..36].copy_from_slice(&[0xaa, 0xbb]);
        frame[36..40].copy_from_slice(&[1, 2, 3, 4]);

        let policy = crate::HardwareDecryptPolicy {
            receive_protected: true,
            pairwise_ccmp: true,
            group_ccmp: false,
        };
        let mut replay = [crate::CcmpReplayWindow::new(); 9];
        replay[3].last_seen = 5;
        let mut duplicates = crate::RxDuplicateState::new();
        let result = process_rx_mpdu(
            &payload,
            DeviceFamily::Ax210,
            false,
            policy,
            &mut replay,
            &mut duplicates,
        )
        .unwrap();
        let RxMpduOutcome::Deliver(frame) = result else {
            panic!("first A-MSDU subframe must be delivered");
        };
        assert!(frame.hardware_decrypted);
        assert_eq!(frame.frame.len(), 38);
        assert_eq!(frame.frame[34..36], [1, 2]);
    }
}
