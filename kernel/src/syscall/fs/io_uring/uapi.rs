use core::mem::size_of;

use bytemuck::{Pod, Zeroable};
use thekernel_linux_io_uring::{
    PINNED_IORING_OP_LAST, RingLayout, SubmissionOpcodeSupport, classify_submission_opcode,
};

pub(crate) const SQE_SIZE: usize = 64;
#[cfg(test)]
pub(crate) const CQE_SIZE: usize = 16;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub(crate) struct SqRingOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub(crate) struct CqRingOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub flags: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub(crate) struct IoUringParams {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub flags: u32,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    pub features: u32,
    pub wq_fd: u32,
    pub resv: [u32; 3],
    pub sq_off: SqRingOffsets,
    pub cq_off: CqRingOffsets,
}

impl IoUringParams {
    pub(crate) fn from_layout(layout: RingLayout) -> Self {
        let sq = layout.sq_offsets();
        let cq = layout.cq_offsets();
        Self {
            sq_entries: layout.sq_entries(),
            cq_entries: layout.cq_entries(),
            flags: layout.setup_flags().bits(),
            features: layout.features().bits(),
            sq_off: SqRingOffsets {
                head: sq.head(),
                tail: sq.tail(),
                ring_mask: sq.ring_mask(),
                ring_entries: sq.ring_entries(),
                flags: sq.flags(),
                dropped: sq.dropped(),
                array: sq.array().unwrap_or(0),
                ..Default::default()
            },
            cq_off: CqRingOffsets {
                head: cq.head(),
                tail: cq.tail(),
                ring_mask: cq.ring_mask(),
                ring_entries: cq.ring_entries(),
                overflow: cq.overflow(),
                cqes: cq.cqes(),
                flags: cq.flags(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

#[repr(C)]
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable, PartialEq, Eq)]
pub(crate) struct IoUringCqe {
    pub user_data: u64,
    pub res: i32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub(crate) struct IoUringProbeHeader {
    pub last_op: u8,
    pub ops_len: u8,
    pub resv: u16,
    pub resv2: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable, PartialEq, Eq)]
pub(crate) struct IoUringProbeOp {
    pub op: u8,
    pub resv: u8,
    pub flags: u16,
    pub resv2: u32,
}

pub(crate) fn write_probe(output: &mut [u8], requested_operations: u32) {
    const IORING_OP_SUPPORTED: u16 = 1;

    let operations = requested_operations.min(PINNED_IORING_OP_LAST as u32) as usize;
    let header = IoUringProbeHeader {
        last_op: PINNED_IORING_OP_LAST - 1,
        ops_len: operations as u8,
        ..Default::default()
    };
    output.fill(0);
    output[..size_of::<IoUringProbeHeader>()].copy_from_slice(bytemuck::bytes_of(&header));
    for opcode in 0..operations {
        let operation = IoUringProbeOp {
            op: opcode as u8,
            flags: match classify_submission_opcode(opcode as u8) {
                SubmissionOpcodeSupport::Supported(_) => IORING_OP_SUPPORTED,
                SubmissionOpcodeSupport::KnownUnsupported | SubmissionOpcodeSupport::Unknown => 0,
            },
            ..Default::default()
        };
        let start = size_of::<IoUringProbeHeader>() + opcode * size_of::<IoUringProbeOp>();
        output[start..start + size_of::<IoUringProbeOp>()]
            .copy_from_slice(bytemuck::bytes_of(&operation));
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub(crate) struct RawSqe([u8; SQE_SIZE]);

impl RawSqe {
    #[cfg(test)]
    pub(crate) const fn zeroed() -> Self {
        Self([0; SQE_SIZE])
    }

    pub(crate) fn decode(self) -> DecodedSqe {
        let bytes = self.0;
        DecodedSqe {
            opcode: bytes[0],
            flags: bytes[1],
            ioprio: u16::from_ne_bytes(bytes[2..4].try_into().unwrap()),
            fd: i32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            offset: u64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
            address: u64::from_ne_bytes(bytes[16..24].try_into().unwrap()),
            len: u32::from_ne_bytes(bytes[24..28].try_into().unwrap()),
            operation_flags: u32::from_ne_bytes(bytes[28..32].try_into().unwrap()),
            user_data: u64::from_ne_bytes(bytes[32..40].try_into().unwrap()),
            buffer_index: u16::from_ne_bytes(bytes[40..42].try_into().unwrap()),
            personality: u16::from_ne_bytes(bytes[42..44].try_into().unwrap()),
            file_index: i32::from_ne_bytes(bytes[44..48].try_into().unwrap()),
            address3: u64::from_ne_bytes(bytes[48..56].try_into().unwrap()),
            pad2: u64::from_ne_bytes(bytes[56..64].try_into().unwrap()),
        }
    }

    #[cfg(test)]
    fn bytes_mut(&mut self) -> &mut [u8; SQE_SIZE] {
        &mut self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DecodedSqe {
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

const _: () = {
    assert!(core::mem::size_of::<SqRingOffsets>() == 40);
    assert!(core::mem::size_of::<CqRingOffsets>() == 40);
    assert!(core::mem::size_of::<IoUringParams>() == 120);
    assert!(core::mem::size_of::<IoUringProbeHeader>() == 16);
    assert!(core::mem::size_of::<IoUringProbeOp>() == 8);
    assert!(core::mem::size_of::<RawSqe>() == SQE_SIZE);
};

#[cfg(test)]
mod tests {
    use super::*;

    fn write<const N: usize>(sqe: &mut RawSqe, offset: usize, value: [u8; N]) {
        sqe.bytes_mut()[offset..offset + N].copy_from_slice(&value);
    }

    #[test]
    fn uapi_layout_matches_linux_64_bit_abi() {
        assert_eq!(core::mem::size_of::<IoUringCqe>(), CQE_SIZE);
        assert_eq!(core::mem::offset_of!(IoUringCqe, user_data), 0);
        assert_eq!(core::mem::offset_of!(IoUringCqe, res), 8);
        assert_eq!(core::mem::offset_of!(IoUringCqe, flags), 12);
        assert_eq!(core::mem::offset_of!(IoUringParams, sq_off), 40);
        assert_eq!(core::mem::offset_of!(IoUringParams, cq_off), 80);
        assert_eq!(core::mem::offset_of!(SqRingOffsets, user_addr), 32);
        assert_eq!(core::mem::offset_of!(CqRingOffsets, user_addr), 32);
    }

    #[test]
    fn sqe_is_decoded_from_one_stable_byte_snapshot() {
        let mut raw = RawSqe::zeroed();
        raw.bytes_mut()[0] = 22;
        raw.bytes_mut()[1] = 1;
        write(&mut raw, 2, 0x0203_u16.to_ne_bytes());
        write(&mut raw, 4, (-7_i32).to_ne_bytes());
        write(&mut raw, 8, 0x0809_0a0b_0c0d_0e0f_u64.to_ne_bytes());
        write(&mut raw, 16, 0x1011_1213_1415_1617_u64.to_ne_bytes());
        write(&mut raw, 24, 0x1819_1a1b_u32.to_ne_bytes());
        write(&mut raw, 28, 0x1c1d_1e1f_u32.to_ne_bytes());
        write(&mut raw, 32, 0x2021_2223_2425_2627_u64.to_ne_bytes());
        write(&mut raw, 40, 0x2829_u16.to_ne_bytes());
        write(&mut raw, 42, 0x2a2b_u16.to_ne_bytes());
        write(&mut raw, 44, (-13_i32).to_ne_bytes());
        write(&mut raw, 48, 0x3031_3233_3435_3637_u64.to_ne_bytes());
        write(&mut raw, 56, 0x3839_3a3b_3c3d_3e3f_u64.to_ne_bytes());

        assert_eq!(
            raw.decode(),
            DecodedSqe {
                opcode: 22,
                flags: 1,
                ioprio: 0x0203,
                fd: -7,
                offset: 0x0809_0a0b_0c0d_0e0f,
                address: 0x1011_1213_1415_1617,
                len: 0x1819_1a1b,
                operation_flags: 0x1c1d_1e1f,
                user_data: 0x2021_2223_2425_2627,
                buffer_index: 0x2829,
                personality: 0x2a2b,
                file_index: -13,
                address3: 0x3031_3233_3435_3637,
                pad2: 0x3839_3a3b_3c3d_3e3f,
            }
        );
    }

    #[test]
    fn returned_params_are_derived_only_from_resolved_geometry() {
        use thekernel_linux_io_uring::{FeatureFlags, SetupFlags, SetupRequest};

        let proven = FeatureFlags::SINGLE_MMAP.union(FeatureFlags::SUBMIT_STABLE);
        let layout = SetupRequest::new(3, 8, SetupFlags::CQSIZE)
            .resolve(proven)
            .unwrap();
        let params = IoUringParams::from_layout(layout);
        assert_eq!(params.sq_entries, 4);
        assert_eq!(params.cq_entries, 8);
        assert_eq!(params.flags, SetupFlags::CQSIZE.bits());
        assert_eq!(params.features, proven.bits());
        assert_eq!(params.features, layout.features().bits());
        assert_eq!(params.sq_off.head, layout.sq_offsets().head());
        assert_eq!(params.sq_off.array, layout.sq_offsets().array().unwrap());
        assert_eq!(params.cq_off.cqes, layout.cq_offsets().cqes());
        assert_eq!(params.resv, [0; 3]);
    }

    #[test]
    fn probe_zeroes_unknown_records_and_marks_only_supported_operations() {
        let operations = PINNED_IORING_OP_LAST as usize;
        let mut output = [0_u8;
            size_of::<IoUringProbeHeader>()
                + PINNED_IORING_OP_LAST as usize * size_of::<IoUringProbeOp>()];
        write_probe(&mut output, operations as u32);

        let header = bytemuck::pod_read_unaligned::<IoUringProbeHeader>(
            &output[..size_of::<IoUringProbeHeader>()],
        );
        assert_eq!(header.last_op, PINNED_IORING_OP_LAST - 1);
        assert_eq!(header.ops_len, PINNED_IORING_OP_LAST);
        assert_eq!(header.resv, 0);
        assert_eq!(header.resv2, [0; 3]);

        for opcode in 0..operations {
            let start = size_of::<IoUringProbeHeader>() + opcode * size_of::<IoUringProbeOp>();
            let operation = bytemuck::pod_read_unaligned::<IoUringProbeOp>(
                &output[start..start + size_of::<IoUringProbeOp>()],
            );
            assert_eq!(operation.op, opcode as u8);
            assert_eq!(
                operation.flags,
                match classify_submission_opcode(opcode as u8) {
                    SubmissionOpcodeSupport::Supported(_) => 1,
                    SubmissionOpcodeSupport::KnownUnsupported
                    | SubmissionOpcodeSupport::Unknown => 0,
                }
            );
            assert_eq!(operation.resv, 0);
            assert_eq!(operation.resv2, 0);
        }
    }
}
