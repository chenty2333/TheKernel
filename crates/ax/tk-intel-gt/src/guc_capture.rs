// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_capture.c and
// guc_capture_fwif.h. Copyright © 2021-2022 Intel Corporation.

use alloc::vec::Vec;

pub const CAPTURE_TYPE_GLOBAL: u8 = 0;
pub const CAPTURE_TYPE_ENGINE_CLASS: u8 = 1;
pub const CAPTURE_TYPE_ENGINE_INSTANCE: u8 = 2;
pub const CAPTURE_TYPE_MAX: u8 = 3;
pub const CAPTURE_GROUP_FULL: u8 = 0;
pub const CAPTURE_GROUP_PARTIAL: u8 = 1;
pub const CAPTURE_GROUP_TYPE_MAX: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureError {
    InvalidBuffer,
    Empty,
    Misaligned,
    Truncated,
    InvalidType,
}

/// Byte-stream reader for the GuC log's error-capture circular region.
pub struct CaptureBuffer<'a> {
    data: &'a [u8],
    rd: usize,
    wr: usize,
}

impl<'a> CaptureBuffer<'a> {
    pub fn new(data: &'a [u8], rd: usize, wr: usize) -> Result<Self, CaptureError> {
        if data.is_empty() || rd >= data.len() || wr >= data.len() {
            return Err(CaptureError::InvalidBuffer);
        }
        Ok(Self { data, rd, wr })
    }

    /// upstream: intel_guc_capture.c guc_capture_buf_cnt().
    pub fn count(&self) -> usize {
        if self.wr >= self.rd {
            self.wr - self.rd
        } else {
            self.data.len() - self.rd + self.wr
        }
    }

    /// upstream: intel_guc_capture.c guc_capture_buf_cnt_to_end().
    pub fn count_to_end(&self) -> usize {
        if self.rd > self.wr {
            self.data.len() - self.rd
        } else {
            self.wr - self.rd
        }
    }

    fn read_dword(&mut self) -> Result<u32, CaptureError> {
        if self.count() < 4 {
            return Err(CaptureError::Truncated);
        }
        // upstream: intel_guc_capture.c guc_capture_log_remove_dw().
        for _ in 0..2 {
            let available = self.count_to_end();
            if available >= 4 {
                let value = u32::from_le_bytes(
                    self.data[self.rd..self.rd + 4]
                        .try_into()
                        .map_err(|_| CaptureError::Truncated)?,
                );
                self.rd += 4;
                return Ok(value);
            }
            // GuC promises dword aligned records; match upstream's recovery
            // behavior by skipping a non-zero tail before retrying at offset 0.
            self.rd = 0;
        }
        Err(CaptureError::Truncated)
    }

    fn read_words<const N: usize>(&mut self) -> Result<[u32; N], CaptureError> {
        if self.count() < N * 4 {
            return Err(CaptureError::Truncated);
        }
        if self.count_to_end() >= N * 4 {
            let mut words = [0; N];
            for (index, word) in words.iter_mut().enumerate() {
                let offset = self.rd + index * 4;
                *word = u32::from_le_bytes(
                    self.data[offset..offset + 4]
                        .try_into()
                        .map_err(|_| CaptureError::Truncated)?,
                );
            }
            self.rd += N * 4;
            return Ok(words);
        }
        let mut words = [0; N];
        for word in &mut words {
            *word = self.read_dword()?;
        }
        Ok(words)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureRegister {
    pub offset: u32,
    pub value: u32,
    pub flags: u32,
    pub mask: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureList {
    pub vfid: u8,
    pub capture_type: u8,
    pub engine_class: u8,
    pub engine_instance: u8,
    pub lrca: u32,
    pub guc_id: u32,
    pub registers: Vec<CaptureRegister>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureGroup {
    pub vfid: u8,
    pub group_type: u8,
    pub lists: Vec<CaptureList>,
}

/// Decode a GuC capture group. Captures remain separate, preserving their
/// original order and metadata for the engine-reset/core-dump consumer.
/// upstream: intel_guc_capture.c guc_capture_extract_reglists().
pub fn extract_group(buffer: &mut CaptureBuffer<'_>) -> Result<CaptureGroup, CaptureError> {
    let available = buffer.count();
    if available == 0 {
        return Err(CaptureError::Empty);
    }
    if available % 4 != 0 {
        return Err(CaptureError::Misaligned);
    }
    // upstream: guc_capture_log_get_group_hdr().
    let [group_owner, group_info] = buffer.read_words::<2>()?;
    let group_type = ((group_info >> 8) & 0xff) as u8;
    if group_type >= CAPTURE_GROUP_TYPE_MAX {
        return Err(CaptureError::InvalidType);
    }
    let num_lists = (group_info & 0xff) as usize;
    let mut lists = Vec::new();
    lists
        .try_reserve_exact(num_lists)
        .map_err(|_| CaptureError::InvalidBuffer)?;

    for _ in 0..num_lists {
        // upstream: guc_capture_log_get_data_hdr().
        let [owner, info, lrca, guc_id, count_word] = buffer.read_words::<5>()?;
        let capture_type = (info & 0xf) as u8;
        let num_registers = (count_word & 0x3ff) as usize;
        let mut registers = Vec::new();
        if capture_type < CAPTURE_TYPE_MAX {
            registers
                .try_reserve_exact(num_registers)
                .map_err(|_| CaptureError::InvalidBuffer)?;
        }
        for _ in 0..num_registers {
            // upstream: guc_capture_log_get_register().
            let [offset, value, flags, mask] = buffer.read_words::<4>()?;
            if capture_type < CAPTURE_TYPE_MAX {
                registers.push(CaptureRegister {
                    offset,
                    value,
                    flags,
                    mask,
                });
            }
        }
        if capture_type < CAPTURE_TYPE_MAX {
            lists.push(CaptureList {
                vfid: (owner & 0xff) as u8,
                capture_type,
                engine_class: ((info >> 4) & 0xf) as u8,
                engine_instance: ((info >> 8) & 0xf) as u8,
                lrca,
                guc_id,
                registers,
            });
        }
    }
    Ok(CaptureGroup {
        vfid: (group_owner & 0xff) as u8,
        group_type,
        lists,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words_to_bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    #[test]
    fn capture_group_extracts_register_metadata() {
        let mut bytes = words_to_bytes(&[
            3,
            1 << 8 | 1, // partial group, one capture
            7,
            2 << 4 | 1 << 8 | 2, // vfid, instance, class 2
            0x1000,
            9,
            1, // LRCA, GuC id, one MMIO
            0x1234,
            0x5678,
            3,
            0xffff,
        ]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let mut ring = CaptureBuffer::new(&bytes, 0, bytes.len() - 4).unwrap();
        let group = extract_group(&mut ring).unwrap();
        assert_eq!(group.vfid, 3);
        assert_eq!(group.group_type, CAPTURE_GROUP_PARTIAL);
        assert_eq!(group.lists.len(), 1);
        assert_eq!(group.lists[0].vfid, 7);
        assert_eq!(group.lists[0].engine_class, 2);
        assert_eq!(group.lists[0].engine_instance, 1);
        assert_eq!(group.lists[0].registers[0].offset, 0x1234);
        assert_eq!(group.lists[0].registers[0].value, 0x5678);
        assert_eq!(ring.count(), 0);
    }

    #[test]
    fn capture_buffer_reads_dwords_across_ring_wrap() {
        let bytes = words_to_bytes(&[10, 11, 12, 13]);
        let mut ring = CaptureBuffer::new(&bytes, 12, 8).unwrap();
        assert_eq!(ring.count(), 12);
        assert_eq!(ring.read_dword(), Ok(13));
        assert_eq!(ring.read_dword(), Ok(10));
        assert_eq!(ring.read_dword(), Ok(11));
        assert_eq!(ring.count(), 0);
    }

    #[test]
    fn unknown_capture_types_are_skipped_as_upstream_does() {
        let mut bytes = words_to_bytes(&[
            0, 1, // full group, one capture
            0, 9, // unknown capture type
            0, 0, 1, // one MMIO to skip
            4, 5, 6, 7,
        ]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let mut ring = CaptureBuffer::new(&bytes, 0, bytes.len() - 4).unwrap();
        let group = extract_group(&mut ring).unwrap();
        assert!(group.lists.is_empty());
    }
}
