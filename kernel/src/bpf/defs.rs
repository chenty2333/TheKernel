//! eBPF type definitions, constants, and user-space attribute structures.
//!
//! Since `linux-raw-sys` does not provide eBPF types, all definitions are
//! hand-written to match the Linux kernel ABI.

use bytemuck::{AnyBitPattern, NoUninit};

// ---------------------------------------------------------------------------
// BPF instruction format
// ---------------------------------------------------------------------------

/// eBPF instruction (8 bytes), matching Linux `struct bpf_insn`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern, NoUninit)]
pub struct BpfInsn {
    /// Opcode: encodes operation class, source, and operation.
    pub code: u8,
    /// Low 4 bits: dst register, high 4 bits: src register.
    pub regs: u8,
    /// Signed offset (for jumps and memory access).
    pub off: i16,
    /// Signed 32-bit immediate constant.
    pub imm: i32,
}

const _: [(); 8] = [(); core::mem::size_of::<BpfInsn>()];

impl BpfInsn {
    pub fn class(&self) -> u8 {
        self.code & 0x07
    }

    pub fn op(&self) -> u8 {
        self.code & 0xf0
    }

    pub fn dst_reg(&self) -> u8 {
        self.regs & 0x0f
    }
    pub fn src_reg(&self) -> u8 {
        (self.regs >> 4) & 0x0f
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BpfLdImm64Data {
    Immediate(u64),
    MapFd(i32),
    MapIndex(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BpfInsnAux {
    Basic,
    LdImm64Head(BpfLdImm64Data),
    LdImm64Cont { head: usize },
}

impl BpfInsnAux {
    pub fn is_continuation(self) -> bool {
        matches!(self, Self::LdImm64Cont { .. })
    }

    pub fn ld_imm64_data(self) -> Option<BpfLdImm64Data> {
        match self {
            Self::LdImm64Head(data) => Some(data),
            _ => None,
        }
    }
}

pub fn bpf_jump_delta(insn: &BpfInsn) -> i64 {
    if insn.class() == BPF_CLASS_JMP32 && insn.op() == BPF_OP_JA {
        insn.imm as i64
    } else {
        insn.off as i64
    }
}

// ---------------------------------------------------------------------------
// BPF syscall commands (first argument to bpf(2))
// ---------------------------------------------------------------------------

pub const BPF_MAP_CREATE: u32 = 0;
pub const BPF_MAP_LOOKUP_ELEM: u32 = 1;
pub const BPF_MAP_UPDATE_ELEM: u32 = 2;
pub const BPF_MAP_DELETE_ELEM: u32 = 3;
pub const BPF_MAP_GET_NEXT_KEY: u32 = 4;
pub const BPF_PROG_LOAD: u32 = 5;
pub const BPF_OBJ_PIN: u32 = 6;
pub const BPF_OBJ_GET: u32 = 7;
pub const BPF_PROG_ATTACH: u32 = 8;
pub const BPF_PROG_DETACH: u32 = 9;
pub const BPF_PROG_TEST_RUN: u32 = 10;
pub const BPF_PROG_GET_NEXT_ID: u32 = 11;
pub const BPF_MAP_GET_NEXT_ID: u32 = 12;
pub const BPF_PROG_GET_FD_BY_ID: u32 = 13;
pub const BPF_MAP_GET_FD_BY_ID: u32 = 14;
pub const BPF_OBJ_GET_INFO_BY_FD: u32 = 15;
pub const BPF_MAP_LOOKUP_AND_DELETE_ELEM: u32 = 21;
pub const BPF_MAP_FREEZE: u32 = 22;

// ---------------------------------------------------------------------------
// BPF map types
// ---------------------------------------------------------------------------

pub const BPF_MAP_TYPE_UNSPEC: u32 = 0;
pub const BPF_MAP_TYPE_HASH: u32 = 1;
pub const BPF_MAP_TYPE_ARRAY: u32 = 2;
pub const BPF_MAP_TYPE_PROG_ARRAY: u32 = 3;
pub const BPF_MAP_TYPE_PERF_EVENT_ARRAY: u32 = 4;
pub const BPF_MAP_TYPE_PERCPU_HASH: u32 = 5;
pub const BPF_MAP_TYPE_PERCPU_ARRAY: u32 = 6;
pub const BPF_MAP_TYPE_LRU_HASH: u32 = 9;
pub const BPF_MAP_TYPE_RINGBUF: u32 = 27;

// ---------------------------------------------------------------------------
// BPF program types
// ---------------------------------------------------------------------------

pub const BPF_PROG_TYPE_UNSPEC: u32 = 0;
pub const BPF_PROG_TYPE_SOCKET_FILTER: u32 = 1;
pub const BPF_PROG_TYPE_KPROBE: u32 = 2;
pub const BPF_PROG_TYPE_SCHED_CLS: u32 = 3;
pub const BPF_PROG_TYPE_SCHED_ACT: u32 = 4;
pub const BPF_PROG_TYPE_TRACEPOINT: u32 = 5;
pub const BPF_PROG_TYPE_XDP: u32 = 6;
pub const BPF_PROG_TYPE_RAW_TRACEPOINT: u32 = 17;

// ---------------------------------------------------------------------------
// BPF map update flags
// ---------------------------------------------------------------------------

pub const BPF_ANY: u64 = 0;
pub const BPF_NOEXIST: u64 = 1;
pub const BPF_EXIST: u64 = 2;

// ---------------------------------------------------------------------------
// BPF helper function IDs
// ---------------------------------------------------------------------------

pub const BPF_FUNC_UNSPEC: u32 = 0;
pub const BPF_FUNC_MAP_LOOKUP_ELEM: u32 = 1;
pub const BPF_FUNC_MAP_UPDATE_ELEM: u32 = 2;
pub const BPF_FUNC_MAP_DELETE_ELEM: u32 = 3;
pub const BPF_FUNC_PROBE_READ: u32 = 4;
pub const BPF_FUNC_KTIME_GET_NS: u32 = 5;
pub const BPF_FUNC_TRACE_PRINTK: u32 = 6;
pub const BPF_FUNC_GET_PRANDOM_U32: u32 = 7;
pub const BPF_FUNC_GET_SMP_PROCESSOR_ID: u32 = 8;
pub const BPF_FUNC_GET_CURRENT_PID_TGID: u32 = 14;
pub const BPF_FUNC_GET_CURRENT_UID_GID: u32 = 15;
pub const BPF_FUNC_GET_CURRENT_COMM: u32 = 16;

// ---------------------------------------------------------------------------
// eBPF ISA: Instruction class (3 LSBs of opcode)
// ---------------------------------------------------------------------------

pub const BPF_CLASS_LD: u8 = 0x00;
pub const BPF_CLASS_LDX: u8 = 0x01;
pub const BPF_CLASS_ST: u8 = 0x02;
pub const BPF_CLASS_STX: u8 = 0x03;
pub const BPF_CLASS_ALU: u8 = 0x04;
pub const BPF_CLASS_JMP: u8 = 0x05;
pub const BPF_CLASS_JMP32: u8 = 0x06;
pub const BPF_CLASS_ALU64: u8 = 0x07;

// ---------------------------------------------------------------------------
// eBPF ISA: Source modifier (bit 3 of opcode)
// ---------------------------------------------------------------------------

pub const BPF_SRC_K: u8 = 0x00;
pub const BPF_SRC_X: u8 = 0x08;

// ---------------------------------------------------------------------------
// eBPF ISA: ALU/JMP operation codes (bits 4-7 of opcode)
// ---------------------------------------------------------------------------

pub const BPF_OP_ADD: u8 = 0x00;
pub const BPF_OP_SUB: u8 = 0x10;
pub const BPF_OP_MUL: u8 = 0x20;
pub const BPF_OP_DIV: u8 = 0x30;
pub const BPF_OP_OR: u8 = 0x40;
pub const BPF_OP_AND: u8 = 0x50;
pub const BPF_OP_LSH: u8 = 0x60;
pub const BPF_OP_RSH: u8 = 0x70;
pub const BPF_OP_NEG: u8 = 0x80;
pub const BPF_OP_MOD: u8 = 0x90;
pub const BPF_OP_XOR: u8 = 0xa0;
pub const BPF_OP_MOV: u8 = 0xb0;
pub const BPF_OP_ARSH: u8 = 0xc0;
pub const BPF_OP_END: u8 = 0xd0;

// JMP operations (same bit positions as ALU)
pub const BPF_OP_JA: u8 = 0x00;
pub const BPF_OP_JEQ: u8 = 0x10;
pub const BPF_OP_JGT: u8 = 0x20;
pub const BPF_OP_JGE: u8 = 0x30;
pub const BPF_OP_JSET: u8 = 0x40;
pub const BPF_OP_JNE: u8 = 0x50;
pub const BPF_OP_JSGT: u8 = 0x60;
pub const BPF_OP_JSGE: u8 = 0x70;
pub const BPF_OP_CALL: u8 = 0x80;
pub const BPF_OP_EXIT: u8 = 0x90;
pub const BPF_OP_JLT: u8 = 0xa0;
pub const BPF_OP_JLE: u8 = 0xb0;
pub const BPF_OP_JSLT: u8 = 0xc0;
pub const BPF_OP_JSLE: u8 = 0xd0;

// ---------------------------------------------------------------------------
// eBPF ISA: Memory access sizes (bits 3-4 of opcode for LD/LDX/ST/STX)
// ---------------------------------------------------------------------------

pub const BPF_SIZE_W: u8 = 0x00;
pub const BPF_SIZE_H: u8 = 0x08;
pub const BPF_SIZE_B: u8 = 0x10;
pub const BPF_SIZE_DW: u8 = 0x18;

// ---------------------------------------------------------------------------
// eBPF ISA: Memory access modes (bits 5-7 of opcode for LD class)
// ---------------------------------------------------------------------------

pub const BPF_MODE_IMM: u8 = 0x00;
pub const BPF_MODE_ABS: u8 = 0x20;
pub const BPF_MODE_IND: u8 = 0x40;
pub const BPF_MODE_MEM: u8 = 0x60;
pub const BPF_MODE_ATOMIC: u8 = 0xc0;

// ---------------------------------------------------------------------------
// eBPF ISA: Atomic operations (imm field when mode == ATOMIC)
// ---------------------------------------------------------------------------

pub const BPF_ATOMIC_ADD: i32 = 0x00;
pub const BPF_ATOMIC_OR: i32 = 0x40;
pub const BPF_ATOMIC_AND: i32 = 0x50;
pub const BPF_ATOMIC_XOR: i32 = 0xa0;
pub const BPF_ATOMIC_XCHG: i32 = 0xe0 | 0x01;
pub const BPF_ATOMIC_CMPXCHG: i32 = 0xf0 | 0x01;
pub const BPF_ATOMIC_FETCH: i32 = 0x01;

// ---------------------------------------------------------------------------
// Special pseudo-registers for 64-bit immediate loads
// ---------------------------------------------------------------------------

/// src_reg value indicating the imm field is a map fd to be resolved.
pub const BPF_PSEUDO_MAP_FD: u8 = 1;
/// src_reg value indicating the imm field is a map fd + value offset.
pub const BPF_PSEUDO_MAP_VALUE: u8 = 2;

// ---------------------------------------------------------------------------
// Misc constants
// ---------------------------------------------------------------------------

pub const BPF_OBJ_NAME_LEN: usize = 16;
pub const BPF_STACK_SIZE: usize = 512;
pub const BPF_MAX_REGS: usize = 11;
pub const BPF_REG_FP: usize = 10;
pub const BPF_MAX_INSNS: usize = 4096;
pub const BPF_MAX_EXEC_INSNS: usize = 1_000_000;

// ---------------------------------------------------------------------------
// User-space attribute structures for bpf() syscall commands.
// Each command gets its own struct rather than a single union.
// ---------------------------------------------------------------------------

/// Attribute for `BPF_MAP_CREATE`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern, NoUninit)]
pub struct BpfAttrMapCreate {
    pub map_type: u32,
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
    pub map_flags: u32,
    pub inner_map_fd: u32,
    pub numa_node: u32,
    pub map_name: [u8; BPF_OBJ_NAME_LEN],
    pub map_ifindex: u32,
    pub btf_fd: u32,
    pub btf_key_type_id: u32,
    pub btf_value_type_id: u32,
    pub btf_vmlinux_value_type_id: u32,
    pub map_extra: u64,
    pub value_type_btf_obj_fd: i32,
    pub map_token_fd: i32,
    pub excl_prog_hash: u64,
    pub excl_prog_hash_size: u32,
}

/// Attribute for `BPF_MAP_LOOKUP_ELEM`, `BPF_MAP_UPDATE_ELEM`,
/// `BPF_MAP_DELETE_ELEM`, `BPF_MAP_GET_NEXT_KEY`,
/// `BPF_MAP_LOOKUP_AND_DELETE_ELEM`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern, NoUninit)]
pub struct BpfAttrMapElem {
    pub map_fd: u32,
    pub _pad0: u32,
    pub key: u64,
    pub value_or_next_key: u64,
    pub flags: u64,
}

/// Attribute for `BPF_PROG_LOAD`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern, NoUninit)]
pub struct BpfAttrProgLoad {
    pub prog_type: u32,
    pub insn_cnt: u32,
    pub insns: u64,
    pub license: u64,
    pub log_level: u32,
    pub log_size: u32,
    pub log_buf: u64,
    pub kern_version: u32,
    pub prog_flags: u32,
    pub prog_name: [u8; BPF_OBJ_NAME_LEN],
    pub prog_ifindex: u32,
    pub expected_attach_type: u32,
    pub prog_btf_fd: u32,
    pub func_info_rec_size: u32,
    pub func_info: u64,
    pub func_info_cnt: u32,
    pub line_info_rec_size: u32,
    pub line_info: u64,
    pub line_info_cnt: u32,
    pub attach_btf_id: u32,
    pub attach_prog_fd_or_btf_obj_fd: u32,
    pub core_relo_cnt: u32,
    pub fd_array: u64,
    pub core_relos: u64,
    pub core_relo_rec_size: u32,
    pub log_true_size: u32,
    pub prog_token_fd: i32,
    pub fd_array_cnt: u32,
    pub signature: u64,
    pub signature_size: u32,
    pub keyring_id: i32,
}

/// Attribute for `BPF_PROG_TEST_RUN`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern, NoUninit)]
pub struct BpfAttrTestRun {
    pub prog_fd: u32,
    pub retval: u32,
    pub data_size_in: u32,
    pub data_size_out: u32,
    pub data_in: u64,
    pub data_out: u64,
    pub repeat: u32,
    pub duration: u32,
    pub ctx_size_in: u32,
    pub ctx_size_out: u32,
    pub ctx_in: u64,
    pub ctx_out: u64,
    pub flags: u32,
    pub cpu: u32,
    pub batch_size: u32,
    pub _pad0: u32,
}

/// Attribute for `BPF_OBJ_GET_INFO_BY_FD`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern, NoUninit)]
pub struct BpfAttrGetInfoByFd {
    pub bpf_fd: u32,
    pub info_len: u32,
    pub info: u64,
}

/// Info structure returned for a BPF map.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BpfMapInfo {
    pub type_: u32,
    pub id: u32,
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
    pub map_flags: u32,
    pub name: [u8; BPF_OBJ_NAME_LEN],
    pub ifindex: u32,
    pub btf_vmlinux_value_type_id: u32,
    pub netns_dev: u64,
    pub netns_ino: u64,
    pub btf_id: u32,
    pub btf_key_type_id: u32,
    pub btf_value_type_id: u32,
    pub _pad0: u32,
    pub map_extra: u64,
}

/// Info structure returned for a BPF program.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BpfProgInfo {
    pub type_: u32,
    pub id: u32,
    pub tag: [u8; 8],
    pub jited_prog_len: u32,
    pub xlated_prog_len: u32,
    pub jited_prog_insns: u64,
    pub xlated_prog_insns: u64,
    pub load_time: u64,
    pub created_by_uid: u32,
    pub nr_map_ids: u32,
    pub map_ids: u64,
    pub name: [u8; BPF_OBJ_NAME_LEN],
    pub ifindex: u32,
    pub gpl_compatible: u32,
    pub netns_dev: u64,
    pub netns_ino: u64,
    pub nr_jited_ksyms: u32,
    pub nr_jited_func_lens: u32,
    pub jited_ksyms: u64,
    pub jited_func_lens: u64,
    pub btf_id: u32,
    pub func_info_rec_size: u32,
    pub func_info: u64,
    pub nr_func_info: u32,
    pub nr_line_info: u32,
    pub line_info: u64,
    pub jited_line_info: u64,
    pub nr_jited_line_info: u32,
    pub line_info_rec_size: u32,
    pub jited_line_info_rec_size: u32,
    pub nr_prog_tags: u32,
    pub prog_tags: u64,
    pub run_time_ns: u64,
    pub run_cnt: u64,
    pub recursion_misses: u64,
    pub verified_insns: u32,
    pub attach_btf_obj_id: u32,
    pub attach_btf_id: u32,
    pub _pad0: u32,
}
