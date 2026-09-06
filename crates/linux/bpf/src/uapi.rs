//! eBPF type definitions, constants, and user-space attribute structures.
//!
//! Since `linux-raw-sys` does not provide eBPF types, all definitions are
//! hand-written to match the Linux kernel ABI.

use bytemuck::{AnyBitPattern, Pod, Zeroable};

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
pub const BPF_PROG_RUN: u32 = BPF_PROG_TEST_RUN;
pub const BPF_PROG_QUERY: u32 = 16;
pub const BPF_PROG_GET_NEXT_ID: u32 = 11;
pub const BPF_MAP_GET_NEXT_ID: u32 = 12;
pub const BPF_PROG_GET_FD_BY_ID: u32 = 13;
pub const BPF_MAP_GET_FD_BY_ID: u32 = 14;
pub const BPF_OBJ_GET_INFO_BY_FD: u32 = 15;
pub const BPF_MAP_LOOKUP_AND_DELETE_ELEM: u32 = 21;
pub const BPF_MAP_FREEZE: u32 = 22;
pub const BPF_RAW_TRACEPOINT_OPEN: u32 = 17;
pub const BPF_TASK_FD_QUERY: u32 = 20;
pub const BPF_MAP_LOOKUP_BATCH: u32 = 24;
pub const BPF_MAP_LOOKUP_AND_DELETE_BATCH: u32 = 25;
pub const BPF_MAP_UPDATE_BATCH: u32 = 26;
pub const BPF_MAP_DELETE_BATCH: u32 = 27;
pub const BPF_LINK_CREATE: u32 = 28;
pub const BPF_LINK_UPDATE: u32 = 29;
pub const BPF_LINK_GET_FD_BY_ID: u32 = 30;
pub const BPF_LINK_GET_NEXT_ID: u32 = 31;
pub const BPF_ENABLE_STATS: u32 = 32;
pub const BPF_BTF_LOAD: u32 = 18;
pub const BPF_BTF_GET_FD_BY_ID: u32 = 19;
pub const BPF_BTF_GET_NEXT_ID: u32 = 23;
pub const BPF_ITER_CREATE: u32 = 33;
pub const BPF_LINK_DETACH: u32 = 34;
pub const BPF_PROG_BIND_MAP: u32 = 35;
pub const BPF_TOKEN_CREATE: u32 = 36;
pub const BPF_PROG_STREAM_READ_BY_FD: u32 = 37;
pub const BPF_PERF_EVENT: u32 = 41;

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
pub const BPF_MAP_TYPE_LPM_TRIE: u32 = 11;
pub const BPF_MAP_TYPE_SOCKMAP: u32 = 15;
pub const BPF_MAP_TYPE_SOCKHASH: u32 = 18;
pub const BPF_MAP_TYPE_QUEUE: u32 = 22;
pub const BPF_MAP_TYPE_STACK: u32 = 23;
pub const BPF_MAP_TYPE_RINGBUF: u32 = 27;
pub const BPF_MAP_TYPE_STRUCT_OPS: u32 = 26;

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
/// Program invoked from a perf event overflow/sample context.
pub const BPF_PROG_TYPE_PERF_EVENT: u32 = 7;
/// Packet programs attached to a cgroup ingress/egress boundary.
pub const BPF_PROG_TYPE_CGROUP_SKB: u32 = 8;
pub const BPF_PROG_TYPE_RAW_TRACEPOINT: u32 = 17;
pub const BPF_PROG_TYPE_RAW_TRACEPOINT_WRITABLE: u32 = 24;
/// Packet programs attached to the namespace netfilter graph.
pub const BPF_PROG_TYPE_NETFILTER: u32 = 32;

// ---------------------------------------------------------------------------
// BPF map update flags
// ---------------------------------------------------------------------------

pub const BPF_ANY: u64 = 0;
pub const BPF_NOEXIST: u64 = 1;
pub const BPF_EXIST: u64 = 2;
/// Required by Linux for `BPF_MAP_TYPE_LPM_TRIE` allocation.
pub const BPF_F_NO_PREALLOC: u32 = 1;

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
pub const BPF_FUNC_TAIL_CALL: u32 = 12;
pub const BPF_FUNC_GET_CURRENT_PID_TGID: u32 = 14;
pub const BPF_FUNC_GET_CURRENT_UID_GID: u32 = 15;
pub const BPF_FUNC_GET_CURRENT_COMM: u32 = 16;
pub const BPF_FUNC_PERF_EVENT_READ: u32 = 22;
pub const BPF_FUNC_PERF_EVENT_OUTPUT: u32 = 25;
pub const BPF_FUNC_PERF_EVENT_READ_VALUE: u32 = 55;
pub const BPF_FUNC_RINGBUF_OUTPUT: u32 = 130;
pub const BPF_FUNC_RINGBUF_RESERVE: u32 = 131;
pub const BPF_FUNC_RINGBUF_SUBMIT: u32 = 132;
pub const BPF_FUNC_RINGBUF_DISCARD: u32 = 133;

pub const BPF_RB_NO_WAKEUP: u64 = 1;
pub const BPF_RB_FORCE_WAKEUP: u64 = 2;

/// Index-mask and current-CPU selector shared by the perf-event helpers.
pub const BPF_F_INDEX_MASK: u64 = 0xffff_ffff;
pub const BPF_F_CURRENT_CPU: u64 = BPF_F_INDEX_MASK;

/// Stable prefix returned by `bpf_perf_event_read_value()`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct BpfPerfEventValue {
    pub counter: u64,
    pub enabled: u64,
    pub running: u64,
}

pub const BPF_OBJ_NAME_LEN: usize = 16;

/// Restrict a map descriptor returned by `BPF_OBJ_GET` to reads or writes.
/// These are descriptor rights, rather than map creation flags.
pub const BPF_F_RDONLY: u32 = 1 << 3;
pub const BPF_F_WRONLY: u32 = 1 << 4;
/// Interpret `BpfAttrObj::path_fd` exactly as `openat(2)`'s directory FD.
pub const BPF_F_PATH_FD: u32 = 1 << 14;

// ---------------------------------------------------------------------------
// User-space attribute structures for bpf() syscall commands.
// Each command gets its own struct rather than a single union.
// ---------------------------------------------------------------------------

/// Attribute for `BPF_MAP_CREATE`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
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
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrMapElem {
    pub map_fd: u32,
    pub _pad0: u32,
    pub key: u64,
    pub value_or_next_key: u64,
    pub flags: u64,
}

/// Attribute for `BPF_ENABLE_STATS` (v6.18).
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrEnableStats {
    pub stats_type: u32,
}

/// Attribute for `BPF_PROG_LOAD`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
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
#[derive(Debug, Clone, Copy, AnyBitPattern)]
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
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrGetInfoByFd {
    pub bpf_fd: u32,
    pub info_len: u32,
    pub info: u64,
}

/// Attribute for `BPF_RAW_TRACEPOINT_OPEN` (v6.18 includes the cookie tail).
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrRawTracepointOpen {
    pub name: u64,
    pub prog_fd: u32,
    pub _pad: u32,
    pub cookie: u64,
}

/// Attribute for `BPF_TASK_FD_QUERY`.
///
/// The first five fields are inputs.  Linux writes `buf_len` back with the
/// complete (non-NUL-inclusive) name length and then publishes the remaining
/// result fields in place in the original union.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrTaskFdQuery {
    pub pid: u32,
    pub fd: u32,
    pub flags: u32,
    pub buf_len: u32,
    pub buf: u64,
    pub prog_id: u32,
    pub fd_type: u32,
    pub probe_offset: u64,
    pub probe_addr: u64,
}

pub const BPF_FD_TYPE_RAW_TRACEPOINT: u32 = 0;
pub const BPF_FD_TYPE_TRACEPOINT: u32 = 1;
pub const BPF_FD_TYPE_KPROBE: u32 = 2;
pub const BPF_FD_TYPE_KRETPROBE: u32 = 3;
pub const BPF_FD_TYPE_UPROBE: u32 = 4;
pub const BPF_FD_TYPE_URETPROBE: u32 = 5;

/// `BPF_OBJ_PIN` / `BPF_OBJ_GET` object-path request.
///
/// This is the common prefix of Linux's `union bpf_attr` arms.  `pathname`
/// is an aligned userspace pointer to a NUL-terminated byte pathname.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrObj {
    pub pathname: u64,
    pub bpf_fd: u32,
    pub file_flags: u32,
    pub path_fd: i32,
    pub _pad: u32,
}

/// Prefix of Linux's `link_create` union used for perf-event links.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrLinkCreate {
    pub prog_fd: u32,
    pub target_fd: u32,
    pub attach_type: u32,
    pub flags: u32,
    pub bpf_cookie: u64,
}

/// Attribute for `BPF_BTF_LOAD`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrBtfLoad {
    pub btf: u64,
    pub btf_log_buf: u64,
    pub btf_size: u32,
    pub btf_log_size: u32,
    pub btf_log_level: u32,
    pub btf_log_true_size: u32,
    pub btf_flags: u32,
    pub btf_token_fd: i32,
}

/// Attribute for `BPF_ITER_CREATE`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrIterCreate {
    pub link_fd: u32,
    pub flags: u32,
}

/// Attribute for `BPF_TOKEN_CREATE`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrTokenCreate {
    pub flags: u32,
    pub bpffs_fd: u32,
}

/// Attribute for `BPF_PROG_STREAM_READ_BY_FD`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrProgStreamRead {
    pub stream_buf: u64,
    pub stream_buf_len: u32,
    pub stream_id: u32,
    pub prog_fd: u32,
}

/// Attribute for `BPF_PROG_BIND_MAP`.
#[repr(C)]
#[derive(Debug, Clone, Copy, AnyBitPattern)]
pub struct BpfAttrProgBindMap {
    pub prog_fd: u32,
    pub map_fd: u32,
    pub flags: u32,
}

/// Info structure returned for a BPF map.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, bytemuck::NoUninit)]
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
#[derive(Debug, Clone, Copy, Default, bytemuck::NoUninit)]
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

/// Common prefix of Linux `struct bpf_link_info`.  The link-specific union is
/// intentionally omitted until a provider owns that concrete link family;
/// callers may still discover the stable type/id/program tuple.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, bytemuck::NoUninit)]
pub struct BpfLinkInfo {
    pub type_: u32,
    pub id: u32,
    pub prog_id: u32,
    pub _pad0: u32,
    /// Complete 48-byte v6.18 link-specific union.  Providers populate the
    /// arm they own and leave the rest zero, exactly like a zeroed kernel
    /// info record copied through a shorter user buffer.
    pub data: [u8; 48],
}

impl Default for BpfLinkInfo {
    fn default() -> Self {
        Self {
            type_: 0,
            id: 0,
            prog_id: 0,
            _pad0: 0,
            data: [0; 48],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, bytemuck::NoUninit)]
pub struct BpfBtfInfo {
    pub btf: u64,
    pub btf_size: u32,
    pub id: u32,
    pub name: u64,
    pub name_len: u32,
    pub kernel_btf: u32,
}

// These command views mirror arms of Linux's `union bpf_attr`. Keep the
// padding explicit: values are copied from arbitrary userspace bytes, and the
// output info records may be copied back byte-for-byte.
const _: [(); 96] = [(); core::mem::size_of::<BpfAttrMapCreate>()];
const _: [(); 24] = [(); core::mem::size_of::<BpfPerfEventValue>()];
const _: [(); 8] = [(); core::mem::align_of::<BpfPerfEventValue>()];
const _: [(); 64] = [(); core::mem::offset_of!(BpfAttrMapCreate, map_extra)];
const _: [(); 32] = [(); core::mem::size_of::<BpfAttrMapElem>()];
const _: [(); 168] = [(); core::mem::size_of::<BpfAttrProgLoad>()];
const _: [(); 80] = [(); core::mem::size_of::<BpfAttrTestRun>()];
const _: [(); 16] = [(); core::mem::size_of::<BpfAttrGetInfoByFd>()];
const _: [(); 24] = [(); core::mem::size_of::<BpfAttrLinkCreate>()];
const _: [(); 40] = [(); core::mem::size_of::<BpfAttrBtfLoad>()];
const _: [(); 8] = [(); core::mem::size_of::<BpfAttrIterCreate>()];
const _: [(); 8] = [(); core::mem::size_of::<BpfAttrTokenCreate>()];
const _: [(); 88] = [(); core::mem::size_of::<BpfMapInfo>()];
const _: [(); 232] = [(); core::mem::size_of::<BpfProgInfo>()];
const _: [(); 64] = [(); core::mem::size_of::<BpfLinkInfo>()];
const _: [(); 8] = [(); core::mem::align_of::<BpfLinkInfo>()];
const _: [(); 32] = [(); core::mem::size_of::<BpfBtfInfo>()];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BpfCommand, MapProfile, ProgramProfile};

    #[test]
    fn linux_bpf_attr_layouts_match_the_x86_64_uapi() {
        assert_eq!(core::mem::size_of::<BpfAttrMapCreate>(), 96);
        assert_eq!(core::mem::size_of::<BpfPerfEventValue>(), 24);
        assert_eq!(core::mem::align_of::<BpfPerfEventValue>(), 8);
        assert_eq!(core::mem::offset_of!(BpfAttrMapCreate, map_extra), 64);
        assert_eq!(core::mem::size_of::<BpfAttrProgLoad>(), 168);
        assert_eq!(core::mem::size_of::<BpfProgInfo>(), 232);
    }

    #[test]
    fn command_and_profile_admission_use_linux_numbers() {
        assert_eq!(
            BpfCommand::try_from(BPF_MAP_CREATE),
            Ok(BpfCommand::MapCreate)
        );
        assert_eq!(
            MapProfile::try_from(BPF_MAP_TYPE_RINGBUF),
            Ok(MapProfile::RingBuf)
        );
        assert_eq!(
            ProgramProfile::try_from(BPF_PROG_TYPE_TRACEPOINT),
            Ok(ProgramProfile::Tracepoint)
        );
    }
}
