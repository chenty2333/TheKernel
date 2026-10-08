//! RX duplicate/A-MSDU sequence handling from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxvar.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

const TID_COUNT: usize = 8;
const NON_QOS_TID: usize = TID_COUNT;
const FC_TYPE_MASK: u8 = 0x0c;
const FC_TYPE_CONTROL: u8 = 0x04;
const FC_TYPE_DATA: u8 = 0x08;
const FC_SUBTYPE_MASK: u8 = 0xf0;
const FC_SUBTYPE_QOS: u8 = 0x80;
const FC_SUBTYPE_NODATA: u8 = 0x40;
const FC1_RETRY: u8 = 0x08;
const QOS_TID_MASK: u16 = 0x000f;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateError {
    TruncatedFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateResult {
    pub duplicate: bool,
    pub same_sequence: bool,
    pub tid_index: u8,
    pub sequence_number: u16,
    pub subframe_index: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxDuplicateState {
    pub last_sequence: [u16; TID_COUNT + 1],
    pub last_subframe: [u8; TID_COUNT + 1],
}

impl RxDuplicateState {
    pub const fn new() -> Self {
        Self {
            last_sequence: [0; TID_COUNT + 1],
            last_subframe: [0; TID_COUNT + 1],
        }
    }

    /// Drop retry duplicates while permitting following subframes in one A-MSDU.
    // upstream: if_iwx.c iwx_detect_duplicate()
    pub fn check(
        &mut self,
        frame: &[u8],
        amsdu: bool,
        amsdu_subframe_index: u8,
    ) -> Result<DuplicateResult, DuplicateError> {
        if frame.len() < 2 {
            return Err(DuplicateError::TruncatedFrame);
        }
        let frame_type = frame[0] & FC_TYPE_MASK;
        let subtype = frame[0] & FC_SUBTYPE_MASK;
        let has_qos = frame_type == FC_TYPE_DATA && subtype & FC_SUBTYPE_QOS != 0;
        if frame_type == FC_TYPE_CONTROL
            || (has_qos && subtype & FC_SUBTYPE_NODATA != 0)
            || (frame.len() > 4 && frame[4] & 1 != 0)
        {
            return Ok(DuplicateResult {
                duplicate: false,
                same_sequence: false,
                tid_index: NON_QOS_TID as u8,
                sequence_number: 0,
                subframe_index: amsdu_subframe_index,
            });
        }
        if frame.len() < 24 {
            return Err(DuplicateError::TruncatedFrame);
        }
        let tid = if has_qos {
            let qos_offset = 24 + if frame[1] & 0x03 == 0x03 { 6 } else { 0 };
            let qos = frame
                .get(qos_offset..qos_offset + 2)
                .ok_or(DuplicateError::TruncatedFrame)?;
            usize::from(u16::from_le_bytes([qos[0], qos[1]]) & QOS_TID_MASK).min(TID_COUNT)
        } else {
            NON_QOS_TID
        };
        let sequence = u16::from_le_bytes([frame[22], frame[23]]) >> 4;
        let duplicate = frame[1] & FC1_RETRY != 0
            && self.last_sequence[tid] == sequence
            && self.last_subframe[tid] >= amsdu_subframe_index;
        if duplicate {
            return Ok(DuplicateResult {
                duplicate: true,
                same_sequence: false,
                tid_index: tid as u8,
                sequence_number: sequence,
                subframe_index: amsdu_subframe_index,
            });
        }
        let same_sequence = self.last_sequence[tid] == sequence
            && amsdu_subframe_index > self.last_subframe[tid]
            && amsdu;
        self.last_sequence[tid] = sequence;
        self.last_subframe[tid] = amsdu_subframe_index;
        Ok(DuplicateResult {
            duplicate: false,
            same_sequence,
            tid_index: tid as u8,
            sequence_number: sequence,
            subframe_index: amsdu_subframe_index,
        })
    }
}

impl Default for RxDuplicateState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn qos_frame(sequence: u16, tid: u8, retry: bool, multicast: bool) -> alloc::vec::Vec<u8> {
        let mut frame = vec![0; 26];
        frame[0] = FC_TYPE_DATA | FC_SUBTYPE_QOS;
        frame[1] = if retry { FC1_RETRY } else { 0 };
        frame[4] = u8::from(multicast);
        frame[22..24].copy_from_slice(&(sequence << 4).to_le_bytes());
        frame[24..26].copy_from_slice(&u16::from(tid).to_le_bytes());
        frame
    }

    #[test]
    fn retry_sequence_drops_duplicates_but_amsdu_subframes_share_sequence() {
        let mut state = RxDuplicateState::new();
        let first = qos_frame(12, 3, false, false);
        assert_eq!(
            state.check(&first, true, 0).unwrap(),
            DuplicateResult {
                duplicate: false,
                same_sequence: false,
                tid_index: 3,
                sequence_number: 12,
                subframe_index: 0,
            }
        );
        let next_subframe = qos_frame(12, 3, false, false);
        assert!(state.check(&next_subframe, true, 1).unwrap().same_sequence);
        let retry = qos_frame(12, 3, true, false);
        assert!(state.check(&retry, true, 1).unwrap().duplicate);
    }

    #[test]
    fn control_qos_null_and_multicast_bypass_duplicate_window() {
        let mut state = RxDuplicateState::new();
        let mut control = qos_frame(1, 2, true, false);
        control[0] = FC_TYPE_CONTROL;
        assert!(!state.check(&control, false, 0).unwrap().duplicate);
        let mut qos_null = qos_frame(1, 2, true, false);
        qos_null[0] |= FC_SUBTYPE_NODATA;
        assert!(!state.check(&qos_null, false, 0).unwrap().duplicate);
        let multicast = qos_frame(1, 2, true, true);
        assert!(!state.check(&multicast, false, 0).unwrap().duplicate);
    }
}
