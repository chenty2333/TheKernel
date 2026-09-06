//! Sole fixed-layout rseq byte decoder.
#![allow(missing_docs)]
use crate::{RseqArea, RseqCriticalSection, RseqError};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodedRseq {
    Area(RseqArea),
    CriticalSection(RseqCriticalSection),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodePlan {
    pub required_bytes: usize,
    pub alignment: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RseqLayout {
    Area,
    CriticalSection,
}
impl RseqLayout {
    pub const fn plan(self) -> DecodePlan {
        DecodePlan {
            required_bytes: 32,
            alignment: 32,
        }
    }
    pub fn decode(self, bytes: &[u8]) -> Result<DecodedRseq, RseqError> {
        if bytes.len() < self.plan().required_bytes {
            return Err(RseqError::InvalidLength);
        }
        let b = &bytes[..32];
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u64at = |o: usize| {
            u64::from_le_bytes([
                b[o],
                b[o + 1],
                b[o + 2],
                b[o + 3],
                b[o + 4],
                b[o + 5],
                b[o + 6],
                b[o + 7],
            ])
        };
        Ok(match self {
            Self::Area => DecodedRseq::Area(RseqArea {
                cpu_id_start: u32at(0),
                cpu_id: u32at(4),
                rseq_cs: u64at(8),
                flags: u32at(16),
                node_id: u32at(20),
                mm_cid: u32at(24),
            }),
            Self::CriticalSection => DecodedRseq::CriticalSection(RseqCriticalSection::from_raw(
                u32at(0),
                u32at(4),
                u64at(8),
                u64at(16),
                u64at(24),
            )),
        })
    }
}
pub fn decode_area(bytes: &[u8]) -> Result<RseqArea, RseqError> {
    match RseqLayout::Area.decode(bytes)? {
        DecodedRseq::Area(value) => Ok(value),
        _ => unreachable!(),
    }
}
pub fn decode_critical_section(bytes: &[u8]) -> Result<RseqCriticalSection, RseqError> {
    match RseqLayout::CriticalSection.decode(bytes)? {
        DecodedRseq::CriticalSection(value) => Ok(value),
        _ => unreachable!(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bytes_and_tail() {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(decode_area(&bytes).unwrap().cpu_id_start, 7);
        assert_eq!(decode_area(&bytes[..31]), Err(RseqError::InvalidLength));
    }
}
