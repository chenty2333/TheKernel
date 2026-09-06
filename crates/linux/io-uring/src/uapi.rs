//! Linux `io_uring` UAPI wire layouts and codecs.
//!
//! These helpers operate only on copied byte arrays.  They do not dereference
//! userspace, allocate, access shared-ring atomics, or execute requests.

use crate::{
    PINNED_IORING_OP_LAST, RingLayout, SubmissionOpcodeSupport, classify_submission_opcode,
};

/// Size of Linux's x86_64 `struct io_uring_params`.
pub const IO_URING_PARAMS_BYTES: usize = 120;
/// Size of Linux's `struct io_uring_sqe` in the supported ABI.
pub const IO_URING_SQE_BYTES: usize = 64;
/// Size of Linux's `struct io_uring_cqe` in the supported ABI.
pub const IO_URING_CQE_BYTES: usize = 16;
/// Size of Linux's `struct io_sqring_offsets` and `struct io_cqring_offsets`.
pub const IO_URING_RING_OFFSETS_BYTES: usize = 40;
/// Size of Linux's `struct io_uring_probe` header.
pub const IO_URING_PROBE_HEADER_BYTES: usize = 16;
/// Size of one Linux `struct io_uring_probe_op` record.
pub const IO_URING_PROBE_OP_BYTES: usize = 8;
/// `io_uring_probe_op::flags` bit advertising a supported opcode.
pub const IORING_OP_SUPPORTED: u16 = 1;

/// A copied `io_uring_params` input or output, independent of userspace
/// pointer alignment and Rust structure padding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoUringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    reserved: [u32; 3],
    sq_offsets: RingOffsets,
    cq_offsets: RingOffsets,
}

impl IoUringParams {
    /// Decodes one complete x86_64 Linux ABI parameter block.
    pub fn decode(bytes: [u8; IO_URING_PARAMS_BYTES]) -> Self {
        Self {
            sq_entries: read_u32(&bytes, 0),
            cq_entries: read_u32(&bytes, 4),
            flags: read_u32(&bytes, 8),
            sq_thread_cpu: read_u32(&bytes, 12),
            sq_thread_idle: read_u32(&bytes, 16),
            features: read_u32(&bytes, 20),
            wq_fd: read_u32(&bytes, 24),
            reserved: [
                read_u32(&bytes, 28),
                read_u32(&bytes, 32),
                read_u32(&bytes, 36),
            ],
            sq_offsets: RingOffsets::decode(&bytes[40..80]),
            cq_offsets: RingOffsets::decode(&bytes[80..120]),
        }
    }

    /// Encodes the output parameters derived from an admitted ring layout.
    pub fn from_layout(layout: RingLayout) -> Self {
        let sq = layout.sq_offsets();
        let cq = layout.cq_offsets();
        Self {
            sq_entries: layout.sq_entries(),
            cq_entries: layout.cq_entries(),
            flags: layout.setup_flags().bits(),
            sq_thread_cpu: layout.sq_thread_cpu(),
            sq_thread_idle: layout.sq_thread_idle(),
            features: layout.features().bits(),
            wq_fd: 0,
            reserved: [0; 3],
            sq_offsets: RingOffsets::from_words(
                [
                    sq.head(),
                    sq.tail(),
                    sq.ring_mask(),
                    sq.ring_entries(),
                    sq.flags(),
                    sq.dropped(),
                    sq.array().unwrap_or(0),
                    0,
                ],
                0,
            ),
            cq_offsets: RingOffsets::from_words(
                [
                    cq.head(),
                    cq.tail(),
                    cq.ring_mask(),
                    cq.ring_entries(),
                    cq.overflow(),
                    cq.cqes(),
                    cq.flags(),
                    0,
                ],
                0,
            ),
        }
    }

    /// Encodes this complete parameter block without relying on host alignment.
    pub fn encode(self) -> [u8; IO_URING_PARAMS_BYTES] {
        let mut bytes = [0; IO_URING_PARAMS_BYTES];
        write_u32(&mut bytes, 0, self.sq_entries);
        write_u32(&mut bytes, 4, self.cq_entries);
        write_u32(&mut bytes, 8, self.flags);
        write_u32(&mut bytes, 12, self.sq_thread_cpu);
        write_u32(&mut bytes, 16, self.sq_thread_idle);
        write_u32(&mut bytes, 20, self.features);
        write_u32(&mut bytes, 24, self.wq_fd);
        for (index, value) in self.reserved.into_iter().enumerate() {
            write_u32(&mut bytes, 28 + index * 4, value);
        }
        self.sq_offsets.encode(&mut bytes[40..80]);
        self.cq_offsets.encode(&mut bytes[80..120]);
        bytes
    }

    pub const fn cq_entries(self) -> u32 {
        self.cq_entries
    }
    pub const fn flags(self) -> u32 {
        self.flags
    }
    pub const fn sq_thread_cpu(self) -> u32 {
        self.sq_thread_cpu
    }
    pub const fn sq_thread_idle(self) -> u32 {
        self.sq_thread_idle
    }
    pub const fn wq_fd(self) -> u32 {
        self.wq_fd
    }
    pub const fn reserved(self) -> [u32; 3] {
        self.reserved
    }
    pub const fn sq_entries(self) -> u32 {
        self.sq_entries
    }
    pub const fn features(self) -> u32 {
        self.features
    }
    pub const fn sq_offsets(self) -> RingOffsets {
        self.sq_offsets
    }
    pub const fn cq_offsets(self) -> RingOffsets {
        self.cq_offsets
    }
}

/// One decoded raw SQ/CQ offsets block.  The field meaning differs only in
/// slots five through seven, exactly as in Linux UAPI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingOffsets([u32; 8], u64);

impl RingOffsets {
    const fn from_words(words: [u32; 8], user_addr: u64) -> Self {
        Self(words, user_addr)
    }
    fn decode(bytes: &[u8]) -> Self {
        Self::from_words(
            [
                read_u32(bytes, 0),
                read_u32(bytes, 4),
                read_u32(bytes, 8),
                read_u32(bytes, 12),
                read_u32(bytes, 16),
                read_u32(bytes, 20),
                read_u32(bytes, 24),
                read_u32(bytes, 28),
            ],
            read_u64(bytes, 32),
        )
    }
    fn encode(self, bytes: &mut [u8]) {
        for (index, value) in self.0.into_iter().enumerate() {
            write_u32(bytes, index * 4, value);
        }
        write_u64(bytes, 32, self.1);
    }
    pub const fn words(self) -> [u32; 8] {
        self.0
    }
    pub const fn user_addr(self) -> u64 {
        self.1
    }
}

/// Returns the clamped operation-record count for `IORING_REGISTER_PROBE`.
pub const fn probe_operation_count(requested: u32) -> usize {
    if requested > PINNED_IORING_OP_LAST as u32 {
        PINNED_IORING_OP_LAST as usize
    } else {
        requested as usize
    }
}

/// Returns exact bytes required for a probe response after Linux's clamp.
pub const fn probe_output_bytes(requested: u32) -> usize {
    IO_URING_PROBE_HEADER_BYTES + probe_operation_count(requested) * IO_URING_PROBE_OP_BYTES
}

/// Encodes a probe response into an exactly sized or larger caller buffer.
/// Returns `false` without modifying a too-short buffer.
pub fn encode_probe(output: &mut [u8], requested: u32) -> bool {
    let operations = probe_operation_count(requested);
    let required = probe_output_bytes(requested);
    if output.len() < required {
        return false;
    }
    output[..required].fill(0);
    output[0] = PINNED_IORING_OP_LAST - 1;
    output[1] = operations as u8;
    for opcode in 0..operations {
        let start = IO_URING_PROBE_HEADER_BYTES + opcode * IO_URING_PROBE_OP_BYTES;
        output[start] = opcode as u8;
        let flags = match classify_submission_opcode(opcode as u8) {
            SubmissionOpcodeSupport::Supported(_) => IORING_OP_SUPPORTED,
            SubmissionOpcodeSupport::KnownUnsupported | SubmissionOpcodeSupport::Unknown => 0,
        };
        output[start + 2..start + 4].copy_from_slice(&flags.to_le_bytes());
    }
    true
}

/// One stable copied 64-byte SQE wire record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawSubmissionEntry([u8; IO_URING_SQE_BYTES]);

impl RawSubmissionEntry {
    pub const fn new(bytes: [u8; IO_URING_SQE_BYTES]) -> Self {
        Self(bytes)
    }
    pub const fn bytes(self) -> [u8; IO_URING_SQE_BYTES] {
        self.0
    }
    pub fn decode(self) -> DecodedSubmissionEntry {
        let bytes = self.0;
        DecodedSubmissionEntry {
            opcode: bytes[0],
            flags: bytes[1],
            ioprio: read_u16(&bytes, 2),
            fd: read_i32(&bytes, 4),
            offset: read_u64(&bytes, 8),
            address: read_u64(&bytes, 16),
            len: read_u32(&bytes, 24),
            operation_flags: read_u32(&bytes, 28),
            user_data: read_u64(&bytes, 32),
            buffer_index: read_u16(&bytes, 40),
            personality: read_u16(&bytes, 42),
            file_index: read_i32(&bytes, 44),
            address3: read_u64(&bytes, 48),
            pad2: read_u64(&bytes, 56),
        }
    }
}

/// Decoded fields of one raw SQE, before opcode-specific admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedSubmissionEntry {
    pub opcode: u8,
    pub flags: u8,
    pub ioprio: u16,
    pub fd: i32,
    pub offset: u64,
    pub address: u64,
    pub len: u32,
    pub operation_flags: u32,
    pub user_data: u64,
    pub buffer_index: u16,
    pub personality: u16,
    pub file_index: i32,
    pub address3: u64,
    pub pad2: u64,
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}
fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}
fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FeatureFlags, SetupFlags, SetupRequest};

    #[test]
    fn params_layout_and_resolved_output_are_exact() {
        let layout = SetupRequest::new(3, 8, SetupFlags::CQSIZE)
            .resolve(FeatureFlags::SINGLE_MMAP)
            .unwrap();
        let encoded = IoUringParams::from_layout(layout).encode();
        assert_eq!(encoded.len(), IO_URING_PARAMS_BYTES);
        assert_eq!(read_u32(&encoded, 0), 4);
        assert_eq!(read_u32(&encoded, 4), 8);
        assert_eq!(read_u32(&encoded, 40), 0);
        assert_eq!(read_u32(&encoded, 64), 192);
        // io_cqring_offsets has a different field order from SQ offsets:
        // overflow, cqes, then flags (Linux UAPI byte offsets 96..108).
        assert_eq!(read_u32(&encoded, 96), 44);
        assert_eq!(read_u32(&encoded, 100), 64);
        assert_eq!(read_u32(&encoded, 104), 40);
        assert_eq!(
            IoUringParams::decode(encoded).features(),
            FeatureFlags::SINGLE_MMAP.bits()
        );
    }

    #[test]
    fn probe_clamps_and_clears_old_bytes() {
        let mut output = [0xa5; IO_URING_PROBE_HEADER_BYTES + 4 * IO_URING_PROBE_OP_BYTES];
        assert!(encode_probe(&mut output, 4));
        assert_eq!(output[0], PINNED_IORING_OP_LAST - 1);
        assert_eq!(output[1], 4);
        assert_eq!(read_u16(&output, 16 + 2), IORING_OP_SUPPORTED);
        assert_eq!(read_u16(&output, 24 + 2), IORING_OP_SUPPORTED);
        assert!(!encode_probe(&mut output[..3], 4));
    }

    #[test]
    fn raw_sqe_decode_preserves_all_union_words() {
        let mut bytes = [0; IO_URING_SQE_BYTES];
        bytes[0] = 22;
        bytes[1] = 1;
        write_u32(&mut bytes, 4, (-7_i32) as u32);
        write_u64(&mut bytes, 32, 0x1122_3344_5566_7788);
        write_u64(&mut bytes, 48, 0x8877_6655_4433_2211);
        let decoded = RawSubmissionEntry::new(bytes).decode();
        assert_eq!(decoded.opcode, 22);
        assert_eq!(decoded.fd, -7);
        assert_eq!(decoded.user_data, 0x1122_3344_5566_7788);
        assert_eq!(decoded.address3, 0x8877_6655_4433_2211);
    }
}
