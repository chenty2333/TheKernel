use bytemuck::{Pod, Zeroable};

pub(crate) const SQE_SIZE: usize = 64;
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

#[repr(C)]
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
    assert!(core::mem::size_of::<IoUringCqe>() == CQE_SIZE);
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
}
