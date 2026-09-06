#![no_std]
#![forbid(unsafe_code)]
//! Pure `perf_event_open(2)` request validation and record-layout planning.

pub const PERF_ATTR_SIZE_VER0: u32 = 64;
pub const PERF_ATTR_SIZE_VER1: u32 = 72;
pub const PERF_ATTR_SIZE_VER2: u32 = 80;
pub const PERF_ATTR_SIZE_VER3: u32 = 96;
pub const PERF_ATTR_SIZE_VER4: u32 = 104;
pub const PERF_ATTR_SIZE_VER5: u32 = 112;
pub const PERF_ATTR_SIZE_VER6: u32 = 120;
pub const PERF_ATTR_SIZE_VER7: u32 = 128;
pub const PERF_ATTR_SIZE_VER8: u32 = 136;
pub const PERF_ATTR_SIZE_VER9: u32 = 144;
/// Size of the current Linux v7.1 `perf_event_attr` ABI.
pub const PERF_ATTR_SIZE: u32 = PERF_ATTR_SIZE_VER9;
pub const PERF_ATTR_MAX_SIZE: u32 = 4096;
pub const PERF_TYPE_HARDWARE: u32 = 0;
pub const PERF_TYPE_SOFTWARE: u32 = 1;
pub const PERF_TYPE_TRACEPOINT: u32 = 2;
pub const PERF_TYPE_HW_CACHE: u32 = 3;
pub const PERF_TYPE_RAW: u32 = 4;
pub const PERF_TYPE_BREAKPOINT: u32 = 5;
pub const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;
pub const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
pub const PERF_COUNT_HW_CACHE_REFERENCES: u64 = 2;
pub const PERF_COUNT_HW_CACHE_MISSES: u64 = 3;
pub const PERF_COUNT_HW_BRANCH_INSTRUCTIONS: u64 = 4;
pub const PERF_COUNT_HW_BRANCH_MISSES: u64 = 5;
pub const PERF_COUNT_HW_BUS_CYCLES: u64 = 6;
pub const PERF_COUNT_HW_STALLED_CYCLES_FRONTEND: u64 = 7;
pub const PERF_COUNT_HW_STALLED_CYCLES_BACKEND: u64 = 8;
pub const PERF_COUNT_HW_REF_CPU_CYCLES: u64 = 9;
pub const PERF_COUNT_SW_CPU_CLOCK: u64 = 0;
pub const PERF_COUNT_SW_TASK_CLOCK: u64 = 1;
pub const PERF_COUNT_SW_PAGE_FAULTS: u64 = 2;
pub const PERF_COUNT_SW_CONTEXT_SWITCHES: u64 = 3;
pub const PERF_COUNT_SW_CPU_MIGRATIONS: u64 = 4;
pub const PERF_COUNT_SW_PAGE_FAULTS_MIN: u64 = 5;
pub const PERF_COUNT_SW_PAGE_FAULTS_MAJ: u64 = 6;
pub const PERF_COUNT_SW_ALIGNMENT_FAULTS: u64 = 7;
pub const PERF_COUNT_SW_EMULATION_FAULTS: u64 = 8;
pub const PERF_COUNT_SW_DUMMY: u64 = 9;
pub const PERF_COUNT_SW_BPF_OUTPUT: u64 = 10;
pub const PERF_COUNT_SW_CGROUP_SWITCHES: u64 = 11;
pub const PERF_FORMAT_TOTAL_TIME_ENABLED: u64 = 1;
pub const PERF_FORMAT_TOTAL_TIME_RUNNING: u64 = 2;
pub const PERF_FORMAT_ID: u64 = 4;
pub const PERF_FORMAT_GROUP: u64 = 8;
pub const PERF_FORMAT_LOST: u64 = 16;
pub const PERF_FORMAT_ALL: u64 = 31;
/// Read-format fields whose values are fully represented by [`ReadPlan`].
pub const PERF_FORMAT_IMPLEMENTED: u64 = PERF_FORMAT_TOTAL_TIME_ENABLED
    | PERF_FORMAT_TOTAL_TIME_RUNNING
    | PERF_FORMAT_ID
    | PERF_FORMAT_GROUP
    | PERF_FORMAT_LOST;
pub const PERF_SAMPLE_IP: u64 = 1;
pub const PERF_SAMPLE_TID: u64 = 2;
pub const PERF_SAMPLE_TIME: u64 = 4;
pub const PERF_SAMPLE_ADDR: u64 = 8;
pub const PERF_SAMPLE_READ: u64 = 16;
pub const PERF_SAMPLE_CALLCHAIN: u64 = 32;
pub const PERF_SAMPLE_ID: u64 = 64;
pub const PERF_SAMPLE_CPU: u64 = 128;
pub const PERF_SAMPLE_PERIOD: u64 = 256;
pub const PERF_SAMPLE_STREAM_ID: u64 = 512;
pub const PERF_SAMPLE_RAW: u64 = 1024;
pub const PERF_SAMPLE_BRANCH_STACK: u64 = 1 << 11;
pub const PERF_SAMPLE_REGS_USER: u64 = 1 << 12;
pub const PERF_SAMPLE_STACK_USER: u64 = 1 << 13;
pub const PERF_SAMPLE_WEIGHT: u64 = 1 << 14;
pub const PERF_SAMPLE_DATA_SRC: u64 = 1 << 15;
pub const PERF_SAMPLE_IDENTIFIER: u64 = 1 << 16;
pub const PERF_SAMPLE_TRANSACTION: u64 = 1 << 17;
pub const PERF_SAMPLE_REGS_INTR: u64 = 1 << 18;
pub const PERF_SAMPLE_PHYS_ADDR: u64 = 1 << 19;
pub const PERF_SAMPLE_AUX: u64 = 1 << 20;
pub const PERF_SAMPLE_CGROUP: u64 = 1 << 21;
pub const PERF_SAMPLE_DATA_PAGE_SIZE: u64 = 1 << 22;
pub const PERF_SAMPLE_CODE_PAGE_SIZE: u64 = 1 << 23;
pub const PERF_SAMPLE_WEIGHT_STRUCT: u64 = 1 << 24;
pub const PERF_SAMPLE_WEIGHT_TYPE: u64 = PERF_SAMPLE_WEIGHT | PERF_SAMPLE_WEIGHT_STRUCT;
pub const PERF_SAMPLE_ALL: u64 = PERF_SAMPLE_IP
    | PERF_SAMPLE_TID
    | PERF_SAMPLE_TIME
    | PERF_SAMPLE_ADDR
    | PERF_SAMPLE_READ
    | PERF_SAMPLE_CALLCHAIN
    | PERF_SAMPLE_ID
    | PERF_SAMPLE_CPU
    | PERF_SAMPLE_PERIOD
    | PERF_SAMPLE_STREAM_ID
    | PERF_SAMPLE_RAW
    | PERF_SAMPLE_BRANCH_STACK
    | PERF_SAMPLE_REGS_USER
    | PERF_SAMPLE_STACK_USER
    | PERF_SAMPLE_WEIGHT
    | PERF_SAMPLE_DATA_SRC
    | PERF_SAMPLE_IDENTIFIER
    | PERF_SAMPLE_TRANSACTION
    | PERF_SAMPLE_REGS_INTR
    | PERF_SAMPLE_PHYS_ADDR
    | PERF_SAMPLE_AUX
    | PERF_SAMPLE_CGROUP
    | PERF_SAMPLE_DATA_PAGE_SIZE
    | PERF_SAMPLE_CODE_PAGE_SIZE
    | PERF_SAMPLE_WEIGHT_STRUCT;
pub const ATTR_DISABLED: u64 = 1;
pub const ATTR_INHERIT: u64 = 1 << 1;
pub const ATTR_PINNED: u64 = 1 << 2;
pub const ATTR_EXCLUSIVE: u64 = 1 << 3;
pub const ATTR_EXCLUDE_USER: u64 = 1 << 4;
pub const ATTR_EXCLUDE_KERNEL: u64 = 1 << 5;
pub const ATTR_EXCLUDE_HV: u64 = 1 << 6;
pub const ATTR_EXCLUDE_IDLE: u64 = 1 << 7;
pub const ATTR_MMAP: u64 = 1 << 8;
pub const ATTR_COMM: u64 = 1 << 9;
pub const ATTR_FREQ: u64 = 1 << 10;
pub const ATTR_INHERIT_STAT: u64 = 1 << 11;
pub const ATTR_ENABLE_ON_EXEC: u64 = 1 << 12;
pub const ATTR_TASK: u64 = 1 << 13;
pub const ATTR_WATERMARK: u64 = 1 << 14;
pub const ATTR_PRECISE_IP: u64 = 3 << 15;
pub const ATTR_MMAP_DATA: u64 = 1 << 17;
pub const ATTR_SAMPLE_ID_ALL: u64 = 1 << 18;
pub const ATTR_EXCLUDE_HOST: u64 = 1 << 19;
pub const ATTR_EXCLUDE_GUEST: u64 = 1 << 20;
pub const ATTR_EXCLUDE_CALLCHAIN_KERNEL: u64 = 1 << 21;
pub const ATTR_EXCLUDE_CALLCHAIN_USER: u64 = 1 << 22;
pub const ATTR_MMAP2: u64 = 1 << 23;
pub const ATTR_COMM_EXEC: u64 = 1 << 24;
pub const ATTR_USE_CLOCKID: u64 = 1 << 25;
pub const ATTR_CONTEXT_SWITCH: u64 = 1 << 26;
pub const ATTR_WRITE_BACKWARD: u64 = 1 << 27;
pub const ATTR_NAMESPACES: u64 = 1 << 28;
pub const ATTR_KSYMBOL: u64 = 1 << 29;
pub const ATTR_BPF_EVENT: u64 = 1 << 30;
pub const ATTR_AUX_OUTPUT: u64 = 1 << 31;
pub const ATTR_CGROUP: u64 = 1 << 32;
pub const ATTR_TEXT_POKE: u64 = 1 << 33;
pub const ATTR_BUILD_ID: u64 = 1 << 34;
pub const ATTR_INHERIT_THREAD: u64 = 1 << 35;
pub const ATTR_REMOVE_ON_EXEC: u64 = 1 << 36;
pub const ATTR_SIGTRAP: u64 = 1 << 37;
pub const ATTR_DEFER_CALLCHAIN: u64 = 1 << 38;
pub const ATTR_DEFER_OUTPUT: u64 = 1 << 39;
pub const ATTR_ALL: u64 = ATTR_DISABLED
    | ATTR_INHERIT
    | ATTR_PINNED
    | ATTR_EXCLUSIVE
    | ATTR_EXCLUDE_USER
    | ATTR_EXCLUDE_KERNEL
    | ATTR_EXCLUDE_HV
    | ATTR_EXCLUDE_IDLE
    | ATTR_MMAP
    | ATTR_COMM
    | ATTR_FREQ
    | ATTR_INHERIT_STAT
    | ATTR_ENABLE_ON_EXEC
    | ATTR_TASK
    | ATTR_WATERMARK
    | ATTR_PRECISE_IP
    | ATTR_MMAP_DATA
    | ATTR_SAMPLE_ID_ALL
    | ATTR_EXCLUDE_HOST
    | ATTR_EXCLUDE_GUEST
    | ATTR_EXCLUDE_CALLCHAIN_KERNEL
    | ATTR_EXCLUDE_CALLCHAIN_USER
    | ATTR_MMAP2
    | ATTR_COMM_EXEC
    | ATTR_USE_CLOCKID
    | ATTR_CONTEXT_SWITCH
    | ATTR_WRITE_BACKWARD
    | ATTR_NAMESPACES
    | ATTR_KSYMBOL
    | ATTR_BPF_EVENT
    | ATTR_AUX_OUTPUT
    | ATTR_CGROUP
    | ATTR_TEXT_POKE
    | ATTR_BUILD_ID
    | ATTR_INHERIT_THREAD
    | ATTR_REMOVE_ON_EXEC
    | ATTR_SIGTRAP
    | ATTR_DEFER_CALLCHAIN
    | ATTR_DEFER_OUTPUT;
/// Attribute flags whose effects are completely represented by [`PerfOpenPlan`].
///
/// Keep this set deliberately narrower than [`ATTR_ALL`]: known Linux flags must
/// not be accepted until the planner can describe their execution semantics.
pub const ATTR_IMPLEMENTED: u64 = ATTR_DISABLED
    | ATTR_PINNED
    | ATTR_EXCLUSIVE
    | ATTR_EXCLUDE_USER
    | ATTR_EXCLUDE_KERNEL
    | ATTR_MMAP
    | ATTR_COMM
    | ATTR_FREQ
    | ATTR_INHERIT
    | ATTR_INHERIT_THREAD
    | ATTR_ENABLE_ON_EXEC
    | ATTR_REMOVE_ON_EXEC
    | ATTR_TASK
    | ATTR_WATERMARK
    | ATTR_MMAP_DATA
    | ATTR_MMAP2
    | ATTR_COMM_EXEC
    | ATTR_CONTEXT_SWITCH
    | ATTR_SAMPLE_ID_ALL
    | ATTR_PRECISE_IP;
pub const PERF_FLAG_FD_NO_GROUP: u64 = 1;
pub const PERF_FLAG_FD_OUTPUT: u64 = 1 << 1;
pub const PERF_FLAG_PID_CGROUP: u64 = 1 << 2;
pub const PERF_FLAG_FD_CLOEXEC: u64 = 1 << 3;
pub const PERF_OPEN_FLAGS_ALL: u64 =
    PERF_FLAG_FD_NO_GROUP | PERF_FLAG_FD_OUTPUT | PERF_FLAG_PID_CGROUP | PERF_FLAG_FD_CLOEXEC;
/// Open flags whose effects are fully represented by [`PerfOpenPlan`].
pub const PERF_OPEN_FLAGS_IMPLEMENTED: u64 = PERF_OPEN_FLAGS_ALL;
pub const PERF_EVENT_IOC_ENABLE: u32 = 0x2400;
pub const PERF_EVENT_IOC_DISABLE: u32 = 0x2401;
pub const PERF_EVENT_IOC_REFRESH: u32 = 0x2402;
pub const PERF_EVENT_IOC_RESET: u32 = 0x2403;
pub const PERF_EVENT_IOC_PERIOD: u32 = 0x4008_2404;
pub const PERF_EVENT_IOC_SET_OUTPUT: u32 = 0x2405;
/// x86_64 ABI request value; the payload is a user pointer.
pub const PERF_EVENT_IOC_SET_FILTER: u32 = 0x4008_2406;
pub const PERF_EVENT_IOC_ID: u32 = 0x8008_2407;
pub const PERF_EVENT_IOC_SET_BPF: u32 = 0x4004_2408;
pub const PERF_EVENT_IOC_PAUSE_OUTPUT: u32 = 0x4004_2409;
/// x86_64 ABI request value; the payload is a user pointer.
pub const PERF_EVENT_IOC_QUERY_BPF: u32 = 0xc008_240a;
/// x86_64 ABI request value; the payload is a user pointer.
pub const PERF_EVENT_IOC_MODIFY_ATTRIBUTES: u32 = 0x4008_240b;
pub const PERF_IOC_FLAG_GROUP: usize = 1;
pub const PERF_RECORD_MMAP: u32 = 1;
pub const PERF_RECORD_LOST: u32 = 2;
pub const PERF_RECORD_COMM: u32 = 3;
pub const PERF_RECORD_EXIT: u32 = 4;
pub const PERF_RECORD_THROTTLE: u32 = 5;
pub const PERF_RECORD_UNTHROTTLE: u32 = 6;
pub const PERF_RECORD_FORK: u32 = 7;
pub const PERF_RECORD_READ: u32 = 8;
pub const PERF_RECORD_SAMPLE: u32 = 9;
pub const PERF_RECORD_MMAP2: u32 = 10;
pub const PERF_RECORD_AUX: u32 = 11;
pub const PERF_RECORD_ITRACE_START: u32 = 12;
pub const PERF_RECORD_LOST_SAMPLES: u32 = 13;
pub const PERF_RECORD_SWITCH: u32 = 14;
pub const PERF_RECORD_SWITCH_CPU_WIDE: u32 = 15;
pub const PERF_RECORD_NAMESPACES: u32 = 16;
pub const PERF_RECORD_KSYMBOL: u32 = 17;
pub const PERF_RECORD_BPF_EVENT: u32 = 18;
pub const PERF_RECORD_CGROUP: u32 = 19;
pub const PERF_RECORD_TEXT_POKE: u32 = 20;
pub const PERF_RECORD_AUX_OUTPUT_HW_ID: u32 = 21;
pub const PERF_RECORD_CALLCHAIN_DEFERRED: u32 = 22;
pub const PERF_RECORD_MISC_KERNEL: u16 = 1;
pub const PERF_RECORD_MISC_USER: u16 = 2;
pub const PERF_RECORD_MISC_CPUMODE_MASK: u16 = 0x0007;
pub const PERF_RECORD_MISC_CPUMODE_UNKNOWN: u16 = 0;
pub const PERF_RECORD_MISC_HYPERVISOR: u16 = 3;
pub const PERF_RECORD_MISC_GUEST_KERNEL: u16 = 4;
pub const PERF_RECORD_MISC_GUEST_USER: u16 = 5;
pub const PERF_RECORD_MISC_PROC_MAP_PARSE_TIMEOUT: u16 = 1 << 12;
pub const PERF_RECORD_MISC_MMAP_DATA: u16 = 1 << 13;
pub const PERF_RECORD_MISC_COMM_EXEC: u16 = 1 << 13;
pub const PERF_RECORD_MISC_FORK_EXEC: u16 = 1 << 13;
pub const PERF_RECORD_MISC_SWITCH_OUT: u16 = 1 << 13;
pub const PERF_RECORD_MISC_EXACT_IP: u16 = 1 << 14;
pub const PERF_RECORD_MISC_SWITCH_OUT_PREEMPT: u16 = 1 << 14;
pub const PERF_RECORD_MISC_MMAP_BUILD_ID: u16 = 1 << 14;
pub const PERF_RECORD_MISC_EXT_RESERVED: u16 = 1 << 15;
pub const PERF_AUX_FLAG_TRUNCATED: u64 = 0x0001;
pub const PERF_AUX_FLAG_OVERWRITE: u64 = 0x0002;
pub const PERF_AUX_FLAG_PARTIAL: u64 = 0x0004;
pub const PERF_AUX_FLAG_COLLISION: u64 = 0x0008;
pub const PERF_AUX_FLAG_PMU_FORMAT_TYPE_MASK: u64 = 0xff00;
pub const PERF_AUX_ACTION_START_PAUSED: u32 = 1;
pub const PERF_AUX_ACTION_PAUSE: u32 = 1 << 1;
pub const PERF_AUX_ACTION_RESUME: u32 = 1 << 2;
pub const PERF_AUX_ACTION_ALL: u32 =
    PERF_AUX_ACTION_START_PAUSED | PERF_AUX_ACTION_PAUSE | PERF_AUX_ACTION_RESUME;
pub const PERF_RECORD_HEADER_SIZE: usize = 8;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PerfRecordHeader {
    pub kind: u32,
    pub misc: u16,
    pub size: u16,
}

/// Builds a naturally aligned non-sample perf record.  Metadata producers
/// own their Linux-specific payload layout, while this shared helper keeps
/// the common header and eight-byte record-size invariant uniform.
pub fn encode_metadata_record(
    out: &mut [u8],
    kind: u32,
    misc: u16,
    payload: &[u8],
) -> Option<usize> {
    let size = PERF_RECORD_HEADER_SIZE.checked_add(payload.len())?;
    if !size.is_multiple_of(8) || size > u16::MAX as usize {
        return None;
    }
    let header = PerfRecordHeader::new(kind, misc, size as u16);
    let target = out.get_mut(..size)?;
    let header_bytes: &mut [u8; PERF_RECORD_HEADER_SIZE] =
        (&mut target[..PERF_RECORD_HEADER_SIZE]).try_into().ok()?;
    header.encode(header_bytes);
    target[PERF_RECORD_HEADER_SIZE..].copy_from_slice(payload);
    Some(size)
}
impl PerfRecordHeader {
    pub const fn new(kind: u32, misc: u16, size: u16) -> Self {
        Self { kind, misc, size }
    }
    pub fn encode(self, out: &mut [u8; PERF_RECORD_HEADER_SIZE]) {
        out[..4].copy_from_slice(&self.kind.to_ne_bytes());
        out[4..6].copy_from_slice(&self.misc.to_ne_bytes());
        out[6..8].copy_from_slice(&self.size.to_ne_bytes());
    }
}

/// One architectural branch-stack entry in a `PERF_RECORD_SAMPLE` payload.
/// The layout is Linux's `perf_branch_entry`: from, to, then the opaque flag
/// word supplied by the PMU.  Callers must leave flags zero when the hardware
/// does not provide the corresponding branch classification bits.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PerfBranchEntry {
    pub from: u64,
    pub to: u64,
    pub flags: u64,
}

/// Exact fields collected for one `PERF_RECORD_SAMPLE`.  This remains a
/// borrowed view so the NMI-to-task handoff never needs an allocation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PerfSampleFields<'a> {
    pub identifier: u64,
    pub ip: u64,
    pub user: bool,
    pub exact_ip: bool,
    pub time: u64,
    pub cpu: u32,
    pub period: u64,
    pub addr: u64,
    pub pid: u32,
    pub tid: u32,
    pub id: u64,
    pub stream_id: u64,
    pub data_src: u64,
    pub branches: &'a [PerfBranchEntry],
    /// Caller-owned trace/probe payload.  Linux places RAW after the ordered
    /// fixed fields as a u32 size followed by bytes padded to eight.
    pub raw: &'a [u8],
    /// Optional bytes carried by `PERF_SAMPLE_AUX`.  AUX is ordered after
    /// DATA_SRC (and the intervening unsupported Linux fields) as a u64 size
    /// followed by bytes padded to eight.  An empty slice is a valid, exact
    /// representation of a sample for which the independent AUX transport
    /// has no contemporaneous bytes.
    pub aux: &'a [u8],
}

/// Encodes the supported, in-order Linux `PERF_RECORD_SAMPLE` payload.  The
/// caller provides fixed preallocated storage; unsupported requested fields
/// must have been rejected at perf_event_open time rather than silently
/// omitted here.
#[inline]
fn push_sample_u64(out: &mut [u8], cursor: &mut usize, value: u64) -> Option<()> {
    let end = cursor.checked_add(8)?;
    out.get_mut(*cursor..end)?
        .copy_from_slice(&value.to_ne_bytes());
    *cursor = end;
    Some(())
}

pub fn encode_sample_record_fields(
    out: &mut [u8],
    sample_type: u64,
    fields: PerfSampleFields<'_>,
) -> Option<usize> {
    let mut cursor = PERF_RECORD_HEADER_SIZE;
    // This sequence is ABI, not an implementation preference.  In
    // particular IDENTIFIER precedes IP while ID/STREAM_ID follow ADDR.
    if sample_type & PERF_SAMPLE_IDENTIFIER != 0 {
        push_sample_u64(out, &mut cursor, fields.identifier)?;
    }
    if sample_type & PERF_SAMPLE_IP != 0 {
        push_sample_u64(out, &mut cursor, fields.ip)?;
    }
    if sample_type & PERF_SAMPLE_TID != 0 {
        push_sample_u64(
            out,
            &mut cursor,
            (u64::from(fields.tid) << 32) | u64::from(fields.pid),
        )?;
    }
    if sample_type & PERF_SAMPLE_TIME != 0 {
        push_sample_u64(out, &mut cursor, fields.time)?;
    }
    if sample_type & PERF_SAMPLE_ADDR != 0 {
        push_sample_u64(out, &mut cursor, fields.addr)?;
    }
    if sample_type & PERF_SAMPLE_ID != 0 {
        push_sample_u64(out, &mut cursor, fields.id)?;
    }
    if sample_type & PERF_SAMPLE_STREAM_ID != 0 {
        push_sample_u64(out, &mut cursor, fields.stream_id)?;
    }
    if sample_type & PERF_SAMPLE_CPU != 0 {
        push_sample_u64(out, &mut cursor, fields.cpu as u64)?;
    }
    if sample_type & PERF_SAMPLE_PERIOD != 0 {
        push_sample_u64(out, &mut cursor, fields.period)?;
    }
    if sample_type & PERF_SAMPLE_RAW != 0 {
        let raw_len = u32::try_from(fields.raw.len()).ok()?;
        let end = cursor.checked_add(4)?.checked_add(fields.raw.len())?;
        let padded = (end.checked_add(7)?) & !7;
        let target = out.get_mut(cursor..padded)?;
        target[..4].copy_from_slice(&raw_len.to_ne_bytes());
        target[4..4 + fields.raw.len()].copy_from_slice(fields.raw);
        target[4 + fields.raw.len()..].fill(0);
        cursor = padded;
    }
    if sample_type & PERF_SAMPLE_BRANCH_STACK != 0 {
        push_sample_u64(out, &mut cursor, fields.branches.len() as u64)?;
        for branch in fields.branches {
            push_sample_u64(out, &mut cursor, branch.from)?;
            push_sample_u64(out, &mut cursor, branch.to)?;
            push_sample_u64(out, &mut cursor, branch.flags)?;
        }
    }
    if sample_type & PERF_SAMPLE_DATA_SRC != 0 {
        push_sample_u64(out, &mut cursor, fields.data_src)?;
    }
    if sample_type & PERF_SAMPLE_AUX != 0 {
        let aux_len = u64::try_from(fields.aux.len()).ok()?;
        push_sample_u64(out, &mut cursor, aux_len)?;
        let end = cursor.checked_add(fields.aux.len())?;
        let padded = (end.checked_add(7)?) & !7;
        let target = out.get_mut(cursor..padded)?;
        target[..fields.aux.len()].copy_from_slice(fields.aux);
        target[fields.aux.len()..].fill(0);
        cursor = padded;
    }
    let size = u16::try_from(cursor).ok()?;
    let mut header = [0; PERF_RECORD_HEADER_SIZE];
    PerfRecordHeader::new(
        PERF_RECORD_SAMPLE,
        (if fields.user {
            PERF_RECORD_MISC_USER
        } else {
            PERF_RECORD_MISC_KERNEL
        }) | if fields.exact_ip {
            PERF_RECORD_MISC_EXACT_IP
        } else {
            0
        },
        size,
    )
    .encode(&mut header);
    out.get_mut(..PERF_RECORD_HEADER_SIZE)?
        .copy_from_slice(&header);
    Some(cursor)
}

/// Encodes the fixed subset of a Linux `PERF_RECORD_SAMPLE` payload supported
/// by the original sampling adapter and returns its complete record size.
pub fn encode_sample_record(
    out: &mut [u8; 40],
    sample_type: u64,
    ip: u64,
    user: bool,
    time: u64,
    cpu: u32,
    period: u64,
) -> usize {
    encode_sample_record_fields(
        out,
        sample_type,
        PerfSampleFields {
            ip,
            user,
            time,
            cpu,
            period,
            ..PerfSampleFields::default()
        },
    )
    .expect("fixed sample record buffer has sufficient space")
}

pub fn encode_lost_record(out: &mut [u8; 24], id: u64, lost: u64) {
    let mut header = [0; PERF_RECORD_HEADER_SIZE];
    PerfRecordHeader::new(PERF_RECORD_LOST, 0, 24).encode(&mut header);
    out[..PERF_RECORD_HEADER_SIZE].copy_from_slice(&header);
    out[8..16].copy_from_slice(&id.to_ne_bytes());
    out[16..24].copy_from_slice(&lost.to_ne_bytes());
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PerfEventAttrV0 {
    pub event_type: u32,
    pub size: u32,
    pub config: u64,
    pub sample_period: u64,
    pub sample_type: u64,
    pub read_format: u64,
    pub flags: u64,
    pub wakeup_events: u32,
    pub bp_type: u32,
    pub config1: u64,
}
pub const PERF_ATTR_SIZE_OFFSET: usize = core::mem::offset_of!(PerfEventAttrV0, size);
const _: () = assert!(core::mem::size_of::<PerfEventAttrV0>() == PERF_ATTR_SIZE_VER0 as usize);

/// Linux v7.1 perf_event_attr, through ABI version 9.
///
/// C unions and bitfields are represented by their canonical, flat storage
/// fields. Aliases such as sample_freq, wakeup_watermark, bp_addr, config2,
/// and the individual flag bits occupy the corresponding field.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PerfEventAttr {
    pub event_type: u32,
    pub size: u32,
    pub config: u64,
    /// Also sample_freq when ATTR_FREQ is set.
    pub sample_period: u64,
    pub sample_type: u64,
    pub read_format: u64,
    /// Flat representation of Linux's attribute bitfield word.
    pub flags: u64,
    /// Also wakeup_watermark when ATTR_WATERMARK is set.
    pub wakeup_events: u32,
    pub bp_type: u32,
    /// Also bp_addr, kprobe_func, or uprobe_path.
    pub config1: u64,
    /// Also bp_len, kprobe_addr, or probe_offset.
    pub config2: u64,
    pub branch_sample_type: u64,
    pub sample_regs_user: u64,
    pub sample_stack_user: u32,
    pub clockid: i32,
    pub sample_regs_intr: u64,
    pub aux_watermark: u32,
    pub sample_max_stack: u16,
    pub reserved_2: u16,
    pub aux_sample_size: u32,
    /// Flat representation of aux_start_paused and AUX overflow actions.
    pub aux_action: u32,
    pub sig_data: u64,
    pub config3: u64,
    pub config4: u64,
}

/// Explicit name for the latest published attr prefix. PerfEventAttrV0
/// remains available for legacy consumers.
pub type PerfEventAttrV9 = PerfEventAttr;

impl From<PerfEventAttrV0> for PerfEventAttr {
    fn from(value: PerfEventAttrV0) -> Self {
        Self {
            event_type: value.event_type,
            size: value.size,
            config: value.config,
            sample_period: value.sample_period,
            sample_type: value.sample_type,
            read_format: value.read_format,
            flags: value.flags,
            wakeup_events: value.wakeup_events,
            bp_type: value.bp_type,
            config1: value.config1,
            ..Self::default()
        }
    }
}

impl From<PerfEventAttr> for PerfEventAttrV0 {
    fn from(value: PerfEventAttr) -> Self {
        Self {
            event_type: value.event_type,
            size: value.size,
            config: value.config,
            sample_period: value.sample_period,
            sample_type: value.sample_type,
            read_format: value.read_format,
            flags: value.flags,
            wakeup_events: value.wakeup_events,
            bp_type: value.bp_type,
            config1: value.config1,
        }
    }
}

pub const PERF_ATTR_CONFIG_OFFSET: usize = core::mem::offset_of!(PerfEventAttr, config);
pub const PERF_ATTR_SAMPLE_PERIOD_OFFSET: usize =
    core::mem::offset_of!(PerfEventAttr, sample_period);
pub const PERF_ATTR_CONFIG1_OFFSET: usize = core::mem::offset_of!(PerfEventAttr, config1);
pub const PERF_ATTR_CONFIG2_OFFSET: usize = core::mem::offset_of!(PerfEventAttr, config2);
pub const PERF_ATTR_BRANCH_SAMPLE_TYPE_OFFSET: usize =
    core::mem::offset_of!(PerfEventAttr, branch_sample_type);
pub const PERF_ATTR_SAMPLE_REGS_USER_OFFSET: usize =
    core::mem::offset_of!(PerfEventAttr, sample_regs_user);
pub const PERF_ATTR_SAMPLE_REGS_INTR_OFFSET: usize =
    core::mem::offset_of!(PerfEventAttr, sample_regs_intr);
pub const PERF_ATTR_AUX_WATERMARK_OFFSET: usize =
    core::mem::offset_of!(PerfEventAttr, aux_watermark);
pub const PERF_ATTR_SIG_DATA_OFFSET: usize = core::mem::offset_of!(PerfEventAttr, sig_data);
pub const PERF_ATTR_CONFIG3_OFFSET: usize = core::mem::offset_of!(PerfEventAttr, config3);
pub const PERF_ATTR_CONFIG4_OFFSET: usize = core::mem::offset_of!(PerfEventAttr, config4);

const _: () = assert!(core::mem::size_of::<PerfEventAttr>() == PERF_ATTR_SIZE_VER9 as usize);
const _: () = assert!(PERF_ATTR_SIZE_OFFSET == 4);
const _: () = assert!(PERF_ATTR_CONFIG_OFFSET == 8);
const _: () = assert!(PERF_ATTR_SAMPLE_PERIOD_OFFSET == 16);
const _: () = assert!(PERF_ATTR_CONFIG1_OFFSET == 56);
const _: () = assert!(PERF_ATTR_CONFIG2_OFFSET == 64);
const _: () = assert!(PERF_ATTR_BRANCH_SAMPLE_TYPE_OFFSET == 72);
const _: () = assert!(PERF_ATTR_SAMPLE_REGS_USER_OFFSET == 80);
const _: () = assert!(PERF_ATTR_SAMPLE_REGS_INTR_OFFSET == 96);
const _: () = assert!(PERF_ATTR_AUX_WATERMARK_OFFSET == 104);
const _: () = assert!(PERF_ATTR_SIG_DATA_OFFSET == 120);
const _: () = assert!(PERF_ATTR_CONFIG3_OFFSET == 128);
const _: () = assert!(PERF_ATTR_CONFIG4_OFFSET == 136);

/// Prefix-size table indexed by Linux perf attr ABI version.
pub const PERF_ATTR_SIZES: [u32; 10] = [
    PERF_ATTR_SIZE_VER0,
    PERF_ATTR_SIZE_VER1,
    PERF_ATTR_SIZE_VER2,
    PERF_ATTR_SIZE_VER3,
    PERF_ATTR_SIZE_VER4,
    PERF_ATTR_SIZE_VER5,
    PERF_ATTR_SIZE_VER6,
    PERF_ATTR_SIZE_VER7,
    PERF_ATTR_SIZE_VER8,
    PERF_ATTR_SIZE_VER9,
];

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PerfEventQueryBpf {
    pub ids_len: u32,
    pub prog_cnt: u32,
}

/// Fixed x86_64 layout of the perf mmap metadata page. The kernel's reserved
/// extension area makes the data ring fields start at byte 1024.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PerfEventMmapPage {
    pub version: u32,
    pub compat_version: u32,
    pub lock: u32,
    pub index: u32,
    pub offset: i64,
    pub time_enabled: u64,
    pub time_running: u64,
    /// Flat representation of the Linux capability bitfield.
    pub capabilities: u64,
    pub pmc_width: u16,
    pub time_shift: u16,
    pub time_mult: u32,
    pub time_offset: u64,
    pub time_zero: u64,
    pub size: u32,
    pub reserved_1: u32,
    pub time_cycles: u64,
    pub time_mask: u64,
    pub reserved: [u8; 116 * 8],
    pub data_head: u64,
    pub data_tail: u64,
    pub data_offset: u64,
    pub data_size: u64,
    pub aux_head: u64,
    pub aux_tail: u64,
    pub aux_offset: u64,
    pub aux_size: u64,
}

pub const PERF_PMU_CAP_USER_RDPMC: u64 = 1 << 2;
pub const PERF_PMU_CAP_USER_TIME: u64 = 1 << 3;
pub const PERF_PMU_CAP_USER_TIME_ZERO: u64 = 1 << 4;
pub const PERF_PMU_CAP_USER_TIME_SHORT: u64 = 1 << 5;
pub const PERF_MMAP_DATA_HEAD_OFFSET: usize = core::mem::offset_of!(PerfEventMmapPage, data_head);
pub const PERF_MMAP_AUX_HEAD_OFFSET: usize = core::mem::offset_of!(PerfEventMmapPage, aux_head);
const _: () = assert!(PERF_MMAP_DATA_HEAD_OFFSET == 1024);
const _: () = assert!(PERF_MMAP_AUX_HEAD_OFFSET == 1056);
const _: () = assert!(core::mem::size_of::<PerfEventMmapPage>() == 1088);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PerfInput {
    pub attr: PerfEventAttrV0,
    pub supplied_size: u32,
    pub tail_nonzero: bool,
    pub open_flags: u64,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FeatureSet {
    pub hardware: bool,
    pub software: bool,
    pub raw: bool,
    pub tracepoint: bool,
    pub breakpoint: bool,
    pub kprobe: bool,
    pub uprobe: bool,
    pub sampling: bool,
    /// Attribute flags the adapter can honor. Bits outside [`ATTR_IMPLEMENTED`]
    /// are ignored by the planner.
    pub attr_flags: u64,
    /// Read-format fields the adapter can emit. Bits outside
    /// [`PERF_FORMAT_IMPLEMENTED`] are ignored by the planner.
    pub read_format: u64,
    /// Open flags the adapter can honor. Bits outside
    /// [`PERF_OPEN_FLAGS_IMPLEMENTED`] are ignored by the planner.
    pub open_flags: u64,
    /// Sampling record fields the adapter can emit.
    pub sample_type: u64,
    /// A non-zero lower bound for sampling periods.
    pub min_sample_period: u64,
    /// A non-zero upper bound for sampling wakeups.
    pub max_wakeup_events: u32,
    /// Read-format fields the sampling adapter can emit.
    pub sampling_read_format: u64,
    /// Sampling backends that do not implement an event-specific config1.
    pub sampling_requires_zero_config1: bool,
    /// Sampling records are available only for hardware events.
    pub sampling_hardware_only: bool,
}
/// Capacity facts supplied by the embedding PMU adapter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PerfSnapshot {
    /// Zero means no adapter-imposed upper limit.
    pub max_sample_period: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    HardwareCycles,
    HardwareInstructions,
    HardwareCacheReferences,
    HardwareCacheMisses,
    HardwareBranchInstructions,
    HardwareBranchMisses,
    HardwareBusCycles,
    HardwareStalledFrontend,
    HardwareStalledBackend,
    HardwareRefCycles,
    HardwareCache(u64),
    SoftwareCpuClock,
    SoftwareTaskClock,
    SoftwarePageFaults,
    SoftwareCpuMigrations,
    SoftwarePageFaultsMin,
    SoftwarePageFaultsMaj,
    SoftwareContextSwitches,
    SoftwareAlignmentFaults,
    SoftwareEmulationFaults,
    SoftwareDummy,
    SoftwareCgroupSwitches,
    SoftwareBpfOutput,
    Raw(u64),
    Tracepoint(u64),
    Breakpoint {
        addr: u64,
        len: u64,
        ty: u32,
    },
    Kprobe {
        function: u64,
        offset: u64,
    },
    Uprobe {
        path: u64,
        offset: u64,
        retprobe: bool,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadPlan {
    pub group: bool,
    pub time_enabled: bool,
    pub time_running: bool,
    pub id: bool,
    pub lost: bool,
}
impl ReadPlan {
    pub const fn bits(self) -> u64 {
        (self.group as u64 * PERF_FORMAT_GROUP)
            | (self.time_enabled as u64 * PERF_FORMAT_TOTAL_TIME_ENABLED)
            | (self.time_running as u64 * PERF_FORMAT_TOTAL_TIME_RUNNING)
            | (self.id as u64 * PERF_FORMAT_ID)
            | (self.lost as u64 * PERF_FORMAT_LOST)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SampleRecordPlan {
    pub period: u64,
    pub sample_type: u64,
    pub fixed_words: u8,
    pub has_raw: bool,
    pub has_callchain: bool,
    pub read: Option<ReadPlan>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PerfOpenPlan {
    pub event: Event,
    pub disabled: bool,
    pub exclude_user: bool,
    pub exclude_kernel: bool,
    pub close_on_exec: bool,
    /// Lifecycle policy frozen from `perf_event_attr` at open time.  The
    /// scheduler owns its application at fork/exec boundaries; keeping it in
    /// the pure ABI plan prevents a descriptor from re-reading mutable user
    /// memory later.
    pub lifecycle: PerfLifecycle,
    pub sample: Option<SampleRecordPlan>,
    pub read: ReadPlan,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PerfLifecycle {
    pub mmap: bool,
    pub comm: bool,
    pub mmap_data: bool,
    pub mmap2: bool,
    pub comm_exec: bool,
    pub context_switch: bool,
    pub namespaces: bool,
    pub ksymbol: bool,
    pub bpf_event: bool,
    pub text_poke: bool,
    pub build_id: bool,
    pub inherit: bool,
    pub inherit_thread: bool,
    pub enable_on_exec: bool,
    pub remove_on_exec: bool,
    pub task: bool,
    pub sample_id_all: bool,
}

impl PerfLifecycle {
    pub const fn from_flags(flags: u64) -> Self {
        Self {
            mmap: flags & ATTR_MMAP != 0,
            comm: flags & ATTR_COMM != 0,
            mmap_data: flags & ATTR_MMAP_DATA != 0,
            mmap2: flags & ATTR_MMAP2 != 0,
            comm_exec: flags & ATTR_COMM_EXEC != 0,
            context_switch: flags & ATTR_CONTEXT_SWITCH != 0,
            namespaces: flags & ATTR_NAMESPACES != 0,
            ksymbol: flags & ATTR_KSYMBOL != 0,
            bpf_event: flags & ATTR_BPF_EVENT != 0,
            text_poke: flags & ATTR_TEXT_POKE != 0,
            build_id: flags & ATTR_BUILD_ID != 0,
            inherit: flags & ATTR_INHERIT != 0,
            inherit_thread: flags & ATTR_INHERIT_THREAD != 0,
            enable_on_exec: flags & ATTR_ENABLE_ON_EXEC != 0,
            remove_on_exec: flags & ATTR_REMOVE_ON_EXEC != 0,
            task: flags & ATTR_TASK != 0,
            sample_id_all: flags & ATTR_SAMPLE_ID_ALL != 0,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reject {
    SizeTooSmall,
    SizeTooLarge,
    NonZeroTail,
    UnknownOpenFlags,
    UnsupportedOpenFlags,
    UnknownAttrFlags,
    UnsupportedAttrFlags,
    UnknownReadFormat,
    UnsupportedReadFormat,
    UnknownSampleType,
    UnsupportedSampleType,
    InvalidExclusion,
    InvalidPeriod,
    InvalidWakeup,
    InvalidBreakpoint,
    InvalidEvent,
    UnsupportedEvent,
    UnsupportedSampling,
    UnsupportedGroup,
    InvalidSamplingMode,
}

pub fn plan(
    input: PerfInput,
    snapshot: PerfSnapshot,
    features: FeatureSet,
) -> Result<PerfOpenPlan, Reject> {
    let n = if input.supplied_size == 0 {
        PERF_ATTR_SIZE_VER0
    } else {
        input.supplied_size
    };
    if n < PERF_ATTR_SIZE_VER0 {
        return Err(Reject::SizeTooSmall);
    }
    if n > PERF_ATTR_MAX_SIZE {
        return Err(Reject::SizeTooLarge);
    }
    // The planner stores only the V0 prefix. Higher-version bytes are legal
    // only when zero: accepting a non-zero extension here would claim a
    // semantic field that no current adapter plan carries.
    if n > PERF_ATTR_SIZE_VER0 && input.tail_nonzero {
        return Err(Reject::NonZeroTail);
    }
    if input.open_flags & !PERF_OPEN_FLAGS_ALL != 0 {
        return Err(Reject::UnknownOpenFlags);
    }
    if input.open_flags & !PERF_OPEN_FLAGS_IMPLEMENTED != 0
        || input.open_flags & !features.open_flags != 0
    {
        return Err(Reject::UnsupportedOpenFlags);
    }
    if input.attr.flags & !ATTR_ALL != 0 {
        return Err(Reject::UnknownAttrFlags);
    }
    if input.attr.flags & !ATTR_IMPLEMENTED != 0 || input.attr.flags & !features.attr_flags != 0 {
        return Err(Reject::UnsupportedAttrFlags);
    }
    if input.attr.read_format & !PERF_FORMAT_ALL != 0 {
        return Err(Reject::UnknownReadFormat);
    }
    if input.attr.read_format & !PERF_FORMAT_IMPLEMENTED != 0
        || input.attr.read_format & !features.read_format != 0
    {
        return Err(Reject::UnsupportedReadFormat);
    }
    if input.attr.sample_type & !PERF_SAMPLE_ALL != 0 {
        return Err(Reject::UnknownSampleType);
    }
    if input.attr.flags & (ATTR_EXCLUDE_USER | ATTR_EXCLUDE_KERNEL)
        == (ATTR_EXCLUDE_USER | ATTR_EXCLUDE_KERNEL)
    {
        return Err(Reject::InvalidExclusion);
    }
    if input.attr.event_type != PERF_TYPE_BREAKPOINT && input.attr.bp_type != 0 {
        return Err(Reject::InvalidBreakpoint);
    }
    let event = match input.attr.event_type {
        PERF_TYPE_HARDWARE => match input.attr.config {
            PERF_COUNT_HW_CPU_CYCLES => Event::HardwareCycles,
            PERF_COUNT_HW_INSTRUCTIONS => Event::HardwareInstructions,
            PERF_COUNT_HW_CACHE_REFERENCES => Event::HardwareCacheReferences,
            PERF_COUNT_HW_CACHE_MISSES => Event::HardwareCacheMisses,
            PERF_COUNT_HW_BRANCH_INSTRUCTIONS => Event::HardwareBranchInstructions,
            PERF_COUNT_HW_BRANCH_MISSES => Event::HardwareBranchMisses,
            PERF_COUNT_HW_BUS_CYCLES => Event::HardwareBusCycles,
            PERF_COUNT_HW_STALLED_CYCLES_FRONTEND => Event::HardwareStalledFrontend,
            PERF_COUNT_HW_STALLED_CYCLES_BACKEND => Event::HardwareStalledBackend,
            PERF_COUNT_HW_REF_CPU_CYCLES => Event::HardwareRefCycles,
            _ => return Err(Reject::InvalidEvent),
        },
        PERF_TYPE_HW_CACHE => Event::HardwareCache(input.attr.config),
        PERF_TYPE_SOFTWARE => match input.attr.config {
            PERF_COUNT_SW_CPU_CLOCK => Event::SoftwareCpuClock,
            PERF_COUNT_SW_TASK_CLOCK => Event::SoftwareTaskClock,
            PERF_COUNT_SW_PAGE_FAULTS => Event::SoftwarePageFaults,
            PERF_COUNT_SW_CONTEXT_SWITCHES => Event::SoftwareContextSwitches,
            PERF_COUNT_SW_CPU_MIGRATIONS => Event::SoftwareCpuMigrations,
            PERF_COUNT_SW_PAGE_FAULTS_MIN => Event::SoftwarePageFaultsMin,
            PERF_COUNT_SW_PAGE_FAULTS_MAJ => Event::SoftwarePageFaultsMaj,
            PERF_COUNT_SW_ALIGNMENT_FAULTS => Event::SoftwareAlignmentFaults,
            PERF_COUNT_SW_EMULATION_FAULTS => Event::SoftwareEmulationFaults,
            PERF_COUNT_SW_DUMMY => Event::SoftwareDummy,
            PERF_COUNT_SW_CGROUP_SWITCHES => Event::SoftwareCgroupSwitches,
            PERF_COUNT_SW_BPF_OUTPUT => Event::SoftwareBpfOutput,
            _ => return Err(Reject::InvalidEvent),
        },
        PERF_TYPE_RAW => Event::Raw(input.attr.config),
        PERF_TYPE_TRACEPOINT => Event::Tracepoint(input.attr.config),
        // The legacy V0 planner intentionally has no config2 member.  These
        // source types are parsed by `plan_attr` from the declared full
        // prefix, rather than manufacturing a truncated descriptor.
        PERF_TYPE_BREAKPOINT => {
            return Err(Reject::UnsupportedEvent);
        }
        _ => return Err(Reject::InvalidEvent),
    };
    if !match event {
        Event::HardwareCycles
        | Event::HardwareInstructions
        | Event::HardwareCacheReferences
        | Event::HardwareCacheMisses
        | Event::HardwareBranchInstructions
        | Event::HardwareBranchMisses
        | Event::HardwareBusCycles
        | Event::HardwareStalledFrontend
        | Event::HardwareStalledBackend
        | Event::HardwareRefCycles
        | Event::HardwareCache(_) => features.hardware,
        Event::SoftwareCpuClock
        | Event::SoftwareTaskClock
        | Event::SoftwarePageFaults
        | Event::SoftwareContextSwitches
        | Event::SoftwareCpuMigrations
        | Event::SoftwarePageFaultsMin
        | Event::SoftwarePageFaultsMaj
        | Event::SoftwareAlignmentFaults
        | Event::SoftwareEmulationFaults
        | Event::SoftwareDummy
        | Event::SoftwareCgroupSwitches => features.software,
        Event::SoftwareBpfOutput => features.software,
        Event::Raw(_) => features.raw,
        Event::Tracepoint(_) => features.tracepoint,
        Event::Breakpoint { .. } => features.breakpoint,
        Event::Kprobe { .. } => features.kprobe,
        Event::Uprobe { .. } => features.uprobe,
    } {
        return Err(Reject::UnsupportedEvent);
    }
    let read = ReadPlan {
        group: input.attr.read_format & PERF_FORMAT_GROUP != 0,
        time_enabled: input.attr.read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0,
        time_running: input.attr.read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0,
        id: input.attr.read_format & PERF_FORMAT_ID != 0,
        lost: input.attr.read_format & PERF_FORMAT_LOST != 0,
    };
    let sampling = input.attr.sample_period != 0 || input.attr.sample_type != 0;
    let bpf_output = matches!(event, Event::SoftwareBpfOutput);
    let sample = if sampling {
        if bpf_output {
            if input.attr.sample_period != 0 || input.attr.sample_type != PERF_SAMPLE_RAW {
                return Err(Reject::InvalidSamplingMode);
            }
            return Ok(PerfOpenPlan {
                event,
                disabled: input.attr.flags & ATTR_DISABLED != 0,
                exclude_user: input.attr.flags & ATTR_EXCLUDE_USER != 0,
                exclude_kernel: input.attr.flags & ATTR_EXCLUDE_KERNEL != 0,
                close_on_exec: input.open_flags & PERF_FLAG_FD_CLOEXEC != 0,
                lifecycle: PerfLifecycle::from_flags(input.attr.flags),
                sample: None,
                read,
            });
        }
        if !features.sampling {
            return Err(Reject::UnsupportedSampling);
        }
        if features.sampling_hardware_only
            && !matches!(event, Event::HardwareCycles | Event::HardwareInstructions)
        {
            return Err(Reject::UnsupportedSampling);
        }
        if input.attr.sample_period == 0 {
            return Err(Reject::InvalidPeriod);
        }
        if input.attr.sample_type == 0 {
            return Err(Reject::InvalidSamplingMode);
        }
        // In frequency mode this word is Hz rather than a hardware period;
        // applying the counter-period floor here would incorrectly reject
        // ordinary perf-record rates such as 99Hz.
        if input.attr.flags & ATTR_FREQ == 0
            && features.min_sample_period != 0
            && input.attr.sample_period < features.min_sample_period
        {
            return Err(Reject::InvalidPeriod);
        }
        if snapshot.max_sample_period != 0 && input.attr.sample_period > snapshot.max_sample_period
        {
            return Err(Reject::InvalidPeriod);
        }
        if input.attr.flags & ATTR_FREQ != 0 && input.attr.sample_period == 0 {
            return Err(Reject::InvalidSamplingMode);
        }
        if features.max_wakeup_events != 0 && input.attr.wakeup_events > features.max_wakeup_events
        {
            return Err(Reject::InvalidWakeup);
        }
        let t = input.attr.sample_type;
        if t & !features.sample_type != 0 {
            return Err(Reject::UnsupportedSampleType);
        }
        if input.attr.read_format & !features.sampling_read_format != 0 {
            return Err(Reject::InvalidSamplingMode);
        }
        if features.sampling_requires_zero_config1 && input.attr.config1 != 0 {
            return Err(Reject::InvalidSamplingMode);
        }
        let words = (t & PERF_SAMPLE_IP != 0) as u8
            + (t & PERF_SAMPLE_TID != 0) as u8
            + (t & PERF_SAMPLE_TIME != 0) as u8
            + (t & PERF_SAMPLE_ADDR != 0) as u8
            + (t & PERF_SAMPLE_ID != 0) as u8
            + (t & PERF_SAMPLE_CPU != 0) as u8
            + (t & PERF_SAMPLE_PERIOD != 0) as u8
            + (t & PERF_SAMPLE_STREAM_ID != 0) as u8
            + (t & PERF_SAMPLE_IDENTIFIER != 0) as u8;
        Some(SampleRecordPlan {
            period: input.attr.sample_period,
            sample_type: t,
            fixed_words: words,
            has_raw: t & PERF_SAMPLE_RAW != 0,
            has_callchain: t & PERF_SAMPLE_CALLCHAIN != 0,
            read: if t & PERF_SAMPLE_READ != 0 {
                Some(read)
            } else {
                None
            },
        })
    } else {
        None
    };
    Ok(PerfOpenPlan {
        event,
        disabled: input.attr.flags & ATTR_DISABLED != 0,
        exclude_user: input.attr.flags & ATTR_EXCLUDE_USER != 0,
        exclude_kernel: input.attr.flags & ATTR_EXCLUDE_KERNEL != 0,
        close_on_exec: input.open_flags & PERF_FLAG_FD_CLOEXEC != 0,
        lifecycle: PerfLifecycle::from_flags(input.attr.flags),
        sample,
        read,
    })
}

/// Describes the target arguments of `perf_event_open(2)` without assigning
/// policy to a file-descriptor table.  Keeping this in the ABI crate makes
/// group, output and cgroup validation testable without making an adapter
/// claim that it can schedule those targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PerfTarget {
    Task { pid: i32, cpu: i32 },
    Cpu { cpu: i32 },
    Cgroup { fd: i32, cpu: i32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PerfOpenTarget {
    pub target: PerfTarget,
    pub group_fd: i32,
    pub output_fd: i32,
    pub open_flags: u64,
}

/// A complete user-buffer view. `extra_tail` is the part following the v9
/// prefix when `supplied_size` is larger than the published structure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PerfAttrInput<'a> {
    pub attr: PerfEventAttr,
    pub supplied_size: u32,
    pub extra_tail: &'a [u8],
    pub target: PerfOpenTarget,
}

/// Capability set for the *schema* planner.  Every non-zero semantic bit is
/// gated by one of these fields; a caller cannot accidentally acquire a
/// capability merely by using a newer attr prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PerfCapabilities {
    pub max_attr_size: u32,
    pub event_types: u64,
    pub attr_flags: u64,
    pub sample_type: u64,
    pub read_format: u64,
    pub open_flags: u64,
    pub branch_sample_type: u64,
    pub regs_user_mask: u64,
    pub regs_intr_mask: u64,
    pub supports_frequency: bool,
    pub supports_watermark: bool,
    pub supports_group: bool,
    pub supports_output: bool,
    pub supports_cgroup: bool,
    pub supports_aux: bool,
    pub supports_sigtrap: bool,
}

impl Default for PerfCapabilities {
    fn default() -> Self {
        Self {
            max_attr_size: PERF_ATTR_SIZE_VER0,
            event_types: 0,
            attr_flags: 0,
            sample_type: 0,
            read_format: 0,
            open_flags: 0,
            branch_sample_type: 0,
            regs_user_mask: 0,
            regs_intr_mask: 0,
            supports_frequency: false,
            supports_watermark: false,
            supports_group: false,
            supports_output: false,
            supports_cgroup: false,
            supports_aux: false,
            supports_sigtrap: false,
        }
    }
}

pub const fn perf_type_bit(event_type: u32) -> u64 {
    if event_type < 64 {
        1u64 << event_type
    } else {
        0
    }
}

/// Full Linux-v7.1 schema. This is intentionally not the kernel adapter
/// capability set; adapters must opt into individual semantic features.
pub const LINUX_V71_SCHEMA: PerfCapabilities = PerfCapabilities {
    max_attr_size: PERF_ATTR_SIZE_VER9,
    event_types: perf_type_bit(PERF_TYPE_HARDWARE)
        | perf_type_bit(PERF_TYPE_SOFTWARE)
        | perf_type_bit(PERF_TYPE_TRACEPOINT)
        | perf_type_bit(PERF_TYPE_RAW)
        | perf_type_bit(PERF_TYPE_BREAKPOINT),
    attr_flags: ATTR_ALL,
    sample_type: PERF_SAMPLE_ALL,
    read_format: PERF_FORMAT_ALL,
    open_flags: PERF_OPEN_FLAGS_ALL,
    branch_sample_type: u64::MAX,
    regs_user_mask: u64::MAX,
    regs_intr_mask: u64::MAX,
    supports_frequency: true,
    supports_watermark: true,
    supports_group: true,
    supports_output: true,
    supports_cgroup: true,
    supports_aux: true,
    supports_sigtrap: true,
};

/// The existing V0 adapter expressed as a schema capability set.  It is a
/// convenience for embedders migrating to [`plan_attr`], and deliberately
/// does not expand what the old `plan` function accepts.
pub const fn narrow_capabilities(features: FeatureSet) -> PerfCapabilities {
    PerfCapabilities {
        max_attr_size: PERF_ATTR_SIZE_VER0,
        event_types: (features.hardware as u64 * perf_type_bit(PERF_TYPE_HARDWARE))
            | (features.software as u64 * perf_type_bit(PERF_TYPE_SOFTWARE))
            | (features.raw as u64 * perf_type_bit(PERF_TYPE_RAW))
            | (features.tracepoint as u64 * perf_type_bit(PERF_TYPE_TRACEPOINT)),
        attr_flags: features.attr_flags & ATTR_IMPLEMENTED,
        sample_type: features.sample_type,
        read_format: features.read_format & PERF_FORMAT_IMPLEMENTED,
        open_flags: features.open_flags & PERF_OPEN_FLAGS_IMPLEMENTED,
        branch_sample_type: 0,
        regs_user_mask: 0,
        regs_intr_mask: 0,
        supports_frequency: false,
        supports_watermark: false,
        supports_group: false,
        supports_output: false,
        supports_cgroup: false,
        supports_aux: false,
        supports_sigtrap: false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplePeriod {
    Period(u64),
    Frequency(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wakeup {
    Events(u32),
    Watermark(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttrExtensions {
    pub config1: u64,
    pub config2: u64,
    pub config3: u64,
    pub config4: u64,
    pub branch_sample_type: u64,
    pub sample_regs_user: u64,
    pub sample_stack_user: u32,
    pub clockid: Option<i32>,
    pub sample_regs_intr: u64,
    pub aux_watermark: u32,
    pub sample_max_stack: u16,
    pub aux_sample_size: u32,
    pub aux_action: u32,
    pub sig_data: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PerfAttrPlan {
    pub event_type: u32,
    pub config: u64,
    pub flags: u64,
    pub lifecycle: PerfLifecycle,
    pub sample_type: u64,
    pub read_format: u64,
    pub period: Option<SamplePeriod>,
    pub wakeup: Wakeup,
    pub target: PerfOpenTarget,
    pub extensions: AttrExtensions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttrReject {
    SizeTooSmall,
    SizeTooLarge,
    NonZeroTail,
    UnsupportedAttrVersion,
    UnknownOpenFlags,
    UnsupportedOpenFlags,
    UnknownAttrFlags,
    UnsupportedAttrFlags,
    UnknownReadFormat,
    UnsupportedReadFormat,
    UnknownSampleType,
    UnsupportedSampleType,
    UnsupportedEvent,
    InvalidTarget,
    UnsupportedTarget,
    InvalidPeriod,
    InvalidWakeup,
    InvalidFlags,
    InvalidExtension,
}

const fn attr_has(size: u32, end: u32) -> bool {
    size >= end
}

/// Parse the Linux v7.1 attr schema.  Fields beyond `supplied_size` are
/// treated exactly as zero; this matters when callers reuse a full Rust
/// structure to model an older userspace prefix.
pub fn plan_attr(
    input: PerfAttrInput<'_>,
    caps: PerfCapabilities,
) -> Result<PerfAttrPlan, AttrReject> {
    let size = if input.supplied_size == 0 {
        PERF_ATTR_SIZE_VER0
    } else {
        input.supplied_size
    };
    if size < PERF_ATTR_SIZE_VER0 {
        return Err(AttrReject::SizeTooSmall);
    }
    if size > PERF_ATTR_MAX_SIZE {
        return Err(AttrReject::SizeTooLarge);
    }
    if size > PERF_ATTR_SIZE && input.extra_tail.iter().any(|&b| b != 0) {
        return Err(AttrReject::NonZeroTail);
    }
    if size > caps.max_attr_size {
        return Err(AttrReject::UnsupportedAttrVersion);
    }
    let a = input.attr;
    if input.target.open_flags & !PERF_OPEN_FLAGS_ALL != 0 {
        return Err(AttrReject::UnknownOpenFlags);
    }
    if input.target.open_flags & !caps.open_flags != 0 {
        return Err(AttrReject::UnsupportedOpenFlags);
    }
    if a.flags & !ATTR_ALL != 0 {
        return Err(AttrReject::UnknownAttrFlags);
    }
    if a.flags & !caps.attr_flags != 0 {
        return Err(AttrReject::UnsupportedAttrFlags);
    }
    if a.read_format & !PERF_FORMAT_ALL != 0 {
        return Err(AttrReject::UnknownReadFormat);
    }
    if a.read_format & !caps.read_format != 0 {
        return Err(AttrReject::UnsupportedReadFormat);
    }
    if a.sample_type & !PERF_SAMPLE_ALL != 0 {
        return Err(AttrReject::UnknownSampleType);
    }
    if a.sample_type & !caps.sample_type != 0 {
        return Err(AttrReject::UnsupportedSampleType);
    }
    if caps.event_types & perf_type_bit(a.event_type) == 0 {
        return Err(AttrReject::UnsupportedEvent);
    }
    let cgroup = input.target.open_flags & PERF_FLAG_PID_CGROUP != 0;
    match input.target.target {
        PerfTarget::Task { pid, cpu } if pid < -1 || cpu < -1 => {
            return Err(AttrReject::InvalidTarget);
        }
        PerfTarget::Cpu { cpu } if cpu < 0 => return Err(AttrReject::InvalidTarget),
        PerfTarget::Cgroup { fd, cpu } if fd < 0 || cpu < -1 || !cgroup => {
            return Err(AttrReject::InvalidTarget);
        }
        PerfTarget::Cgroup { .. } if !caps.supports_cgroup => {
            return Err(AttrReject::UnsupportedTarget);
        }
        _ => {}
    }
    if cgroup && !matches!(input.target.target, PerfTarget::Cgroup { .. }) {
        return Err(AttrReject::InvalidTarget);
    }
    if input.target.group_fd >= 0 && !caps.supports_group {
        return Err(AttrReject::UnsupportedTarget);
    }
    if input.target.output_fd >= 0 {
        if input.target.open_flags & PERF_FLAG_FD_OUTPUT == 0 {
            return Err(AttrReject::InvalidTarget);
        }
        if !caps.supports_output {
            return Err(AttrReject::UnsupportedTarget);
        }
    }
    let frequency = a.flags & ATTR_FREQ != 0;
    if frequency && !caps.supports_frequency {
        return Err(AttrReject::UnsupportedAttrFlags);
    }
    let sampling = a.sample_period != 0 || a.sample_type != 0;
    if sampling && a.sample_period == 0 {
        return Err(AttrReject::InvalidPeriod);
    }
    let period = if sampling {
        Some(if frequency {
            SamplePeriod::Frequency(a.sample_period)
        } else {
            SamplePeriod::Period(a.sample_period)
        })
    } else {
        None
    };
    let watermark = a.flags & ATTR_WATERMARK != 0;
    if watermark && !caps.supports_watermark {
        return Err(AttrReject::UnsupportedAttrFlags);
    }
    if a.flags & ATTR_SIGTRAP != 0 && !caps.supports_sigtrap {
        return Err(AttrReject::UnsupportedAttrFlags);
    }
    let e = AttrExtensions {
        config1: a.config1,
        config2: if attr_has(size, PERF_ATTR_SIZE_VER1) {
            a.config2
        } else {
            0
        },
        branch_sample_type: if attr_has(size, PERF_ATTR_SIZE_VER2) {
            a.branch_sample_type
        } else {
            0
        },
        sample_regs_user: if attr_has(size, PERF_ATTR_SIZE_VER2) {
            a.sample_regs_user
        } else {
            0
        },
        sample_stack_user: if attr_has(size, PERF_ATTR_SIZE_VER3) {
            a.sample_stack_user
        } else {
            0
        },
        clockid: if attr_has(size, PERF_ATTR_SIZE_VER3) && a.flags & ATTR_USE_CLOCKID != 0 {
            Some(a.clockid)
        } else {
            None
        },
        sample_regs_intr: if attr_has(size, PERF_ATTR_SIZE_VER4) {
            a.sample_regs_intr
        } else {
            0
        },
        aux_watermark: if attr_has(size, PERF_ATTR_SIZE_VER5) {
            a.aux_watermark
        } else {
            0
        },
        sample_max_stack: if attr_has(size, PERF_ATTR_SIZE_VER5) {
            a.sample_max_stack
        } else {
            0
        },
        aux_sample_size: if attr_has(size, PERF_ATTR_SIZE_VER6) {
            a.aux_sample_size
        } else {
            0
        },
        aux_action: if attr_has(size, PERF_ATTR_SIZE_VER6) {
            a.aux_action
        } else {
            0
        },
        sig_data: if attr_has(size, PERF_ATTR_SIZE_VER7) {
            a.sig_data
        } else {
            0
        },
        config3: if attr_has(size, PERF_ATTR_SIZE_VER8) {
            a.config3
        } else {
            0
        },
        config4: if attr_has(size, PERF_ATTR_SIZE_VER9) {
            a.config4
        } else {
            0
        },
    };
    if e.branch_sample_type & !caps.branch_sample_type != 0
        || e.sample_regs_user & !caps.regs_user_mask != 0
        || e.sample_regs_intr & !caps.regs_intr_mask != 0
    {
        return Err(AttrReject::InvalidExtension);
    }
    if e.aux_action & !PERF_AUX_ACTION_ALL != 0 {
        return Err(AttrReject::InvalidExtension);
    }
    if (e.aux_watermark != 0
        || e.aux_sample_size != 0
        || e.aux_action != 0
        || a.flags & ATTR_AUX_OUTPUT != 0)
        && !caps.supports_aux
    {
        return Err(AttrReject::UnsupportedAttrFlags);
    }
    Ok(PerfAttrPlan {
        event_type: a.event_type,
        config: a.config,
        flags: a.flags,
        lifecycle: PerfLifecycle::from_flags(a.flags),
        sample_type: a.sample_type,
        read_format: a.read_format,
        period,
        wakeup: if watermark {
            Wakeup::Watermark(a.wakeup_events)
        } else {
            Wakeup::Events(a.wakeup_events)
        },
        target: input.target,
        extensions: e,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecError {
    Truncated,
    BadSize,
    Misaligned,
    Overflow,
    WrongKind,
}

pub fn decode_record_header(bytes: &[u8]) -> Result<PerfRecordHeader, CodecError> {
    if bytes.len() < PERF_RECORD_HEADER_SIZE {
        return Err(CodecError::Truncated);
    }
    let header = PerfRecordHeader {
        kind: u32::from_ne_bytes(bytes[0..4].try_into().map_err(|_| CodecError::Truncated)?),
        misc: u16::from_ne_bytes(bytes[4..6].try_into().map_err(|_| CodecError::Truncated)?),
        size: u16::from_ne_bytes(bytes[6..8].try_into().map_err(|_| CodecError::Truncated)?),
    };
    if (header.size as usize) < PERF_RECORD_HEADER_SIZE || (header.size as usize) > bytes.len() {
        return Err(CodecError::BadSize);
    }
    if header.size & 7 != 0 {
        return Err(CodecError::Misaligned);
    }
    Ok(header)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuxRecord {
    pub offset: u64,
    pub size: u64,
    pub flags: u64,
}
pub fn encode_aux_record(out: &mut [u8], aux: AuxRecord) -> Result<usize, CodecError> {
    const LEN: usize = 32;
    if out.len() < LEN {
        return Err(CodecError::Truncated);
    }
    PerfRecordHeader::new(PERF_RECORD_AUX, 0, LEN as u16).encode(
        (&mut out[..8])
            .try_into()
            .map_err(|_| CodecError::Truncated)?,
    );
    out[8..16].copy_from_slice(&aux.offset.to_ne_bytes());
    out[16..24].copy_from_slice(&aux.size.to_ne_bytes());
    out[24..32].copy_from_slice(&aux.flags.to_ne_bytes());
    Ok(LEN)
}
pub fn decode_aux_record(bytes: &[u8]) -> Result<AuxRecord, CodecError> {
    let h = decode_record_header(bytes)?;
    if h.kind != PERF_RECORD_AUX || h.size != 32 {
        return Err(CodecError::WrongKind);
    }
    Ok(AuxRecord {
        offset: u64::from_ne_bytes(bytes[8..16].try_into().map_err(|_| CodecError::Truncated)?),
        size: u64::from_ne_bytes(
            bytes[16..24]
                .try_into()
                .map_err(|_| CodecError::Truncated)?,
        ),
        flags: u64::from_ne_bytes(
            bytes[24..32]
                .try_into()
                .map_err(|_| CodecError::Truncated)?,
        ),
    })
}

pub fn checked_read_size(read: ReadPlan, members: usize) -> Result<usize, CodecError> {
    let value_words = if read.group {
        let words_per_member = 1 + read.id as usize + read.lost as usize;
        members
            .checked_mul(words_per_member)
            .and_then(|words| words.checked_add(1))
            .ok_or(CodecError::Overflow)?
    } else {
        1
    };
    let suffix = read.time_enabled as usize
        + read.time_running as usize
        + (!read.group && read.id) as usize
        + (!read.group && read.lost) as usize;
    value_words
        .checked_add(suffix)
        .and_then(|n| n.checked_mul(8))
        .ok_or(CodecError::Overflow)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmapRingLayout {
    pub data_offset: u64,
    pub data_size: u64,
    pub aux_offset: u64,
    pub aux_size: u64,
}
impl MmapRingLayout {
    pub fn validate(self) -> Result<(), CodecError> {
        if self.data_offset & 7 != 0 || self.aux_offset & 7 != 0 {
            return Err(CodecError::Misaligned);
        }
        if self.data_size == 0
            || !self.data_size.is_power_of_two()
            || (self.aux_size != 0 && !self.aux_size.is_power_of_two())
        {
            return Err(CodecError::BadSize);
        }
        self.data_offset
            .checked_add(self.data_size)
            .ok_or(CodecError::Overflow)?;
        self.aux_offset
            .checked_add(self.aux_size)
            .ok_or(CodecError::Overflow)?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn i() -> PerfInput {
        PerfInput {
            attr: PerfEventAttrV0 {
                event_type: PERF_TYPE_HARDWARE,
                config: 0,
                size: PERF_ATTR_SIZE,
                ..PerfEventAttrV0::default()
            },
            supplied_size: PERF_ATTR_SIZE,
            tail_nonzero: false,
            open_flags: 0,
        }
    }
    #[test]
    fn sizes_then_tail() {
        let mut x = i();
        x.supplied_size = 63;
        x.tail_nonzero = true;
        assert_eq!(
            plan(x, PerfSnapshot::default(), FeatureSet::default()),
            Err(Reject::SizeTooSmall)
        );
        x.supplied_size = PERF_ATTR_SIZE_VER9 + 1;
        assert_eq!(
            plan(x, PerfSnapshot::default(), FeatureSet::default()),
            Err(Reject::NonZeroTail)
        );
    }

    #[test]
    fn v0_layout_and_uapi_numbers_are_stable() {
        assert_eq!(core::mem::size_of::<PerfEventAttrV0>(), 64);
        assert_eq!(PERF_ATTR_SIZE_OFFSET, 4);
        assert_eq!(PERF_TYPE_HARDWARE, 0);
        assert_eq!(PERF_TYPE_SOFTWARE, 1);
        assert_eq!(PERF_COUNT_HW_CPU_CYCLES, 0);
        assert_eq!(PERF_COUNT_HW_INSTRUCTIONS, 1);
        assert_eq!(PERF_FLAG_FD_CLOEXEC, 1 << 3);
        assert_eq!(PERF_FORMAT_GROUP, 1 << 3);
    }

    #[test]
    fn ver9_attr_prefixes_and_flat_union_storage_match_linux() {
        assert_eq!(
            PERF_ATTR_SIZES,
            [64, 72, 80, 96, 104, 112, 120, 128, 136, 144]
        );
        assert_eq!(core::mem::size_of::<PerfEventAttr>(), 144);
        assert_eq!(PERF_ATTR_CONFIG2_OFFSET, PERF_ATTR_SIZE_VER0 as usize);
        assert_eq!(
            PERF_ATTR_BRANCH_SAMPLE_TYPE_OFFSET,
            PERF_ATTR_SIZE_VER1 as usize
        );
        assert_eq!(
            PERF_ATTR_SAMPLE_REGS_USER_OFFSET,
            PERF_ATTR_SIZE_VER2 as usize
        );
        assert_eq!(
            PERF_ATTR_SAMPLE_REGS_INTR_OFFSET,
            PERF_ATTR_SIZE_VER3 as usize
        );
        assert_eq!(PERF_ATTR_AUX_WATERMARK_OFFSET, PERF_ATTR_SIZE_VER4 as usize);
        assert_eq!(PERF_ATTR_SIG_DATA_OFFSET, PERF_ATTR_SIZE_VER6 as usize);
        assert_eq!(PERF_ATTR_CONFIG3_OFFSET, PERF_ATTR_SIZE_VER7 as usize);
        assert_eq!(PERF_ATTR_CONFIG4_OFFSET, PERF_ATTR_SIZE_VER8 as usize);

        let legacy = PerfEventAttrV0 {
            config1: 0xfeed_beef,
            ..PerfEventAttrV0::default()
        };
        let full = PerfEventAttr::from(legacy);
        assert_eq!(full.config1, 0xfeed_beef);
        assert_eq!(PerfEventAttrV0::from(full), legacy);
    }

    #[test]
    fn complete_sample_bits_are_known_but_not_planner_implemented() {
        assert_eq!(
            PERF_SAMPLE_WEIGHT_TYPE,
            PERF_SAMPLE_WEIGHT | PERF_SAMPLE_WEIGHT_STRUCT
        );
        assert_eq!(PERF_SAMPLE_ALL, (1 << 25) - 1);
        let mut x = i();
        x.attr.sample_period = 1;
        x.attr.wakeup_events = 1;
        x.attr.sample_type = PERF_SAMPLE_BRANCH_STACK;
        assert_eq!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    sampling: true,
                    sample_type: PERF_SAMPLE_IP,
                    ..FeatureSet::default()
                },
            ),
            Err(Reject::UnsupportedSampleType)
        );
    }

    #[test]
    fn nonzero_ver9_extensions_are_not_silently_planned_as_v0() {
        let mut x = i();
        x.supplied_size = PERF_ATTR_SIZE_VER9;
        x.tail_nonzero = true;
        assert_eq!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    ..FeatureSet::default()
                },
            ),
            Err(Reject::NonZeroTail)
        );
    }

    #[test]
    fn mmap_page_and_modern_uapi_numbers_match_x86_64_linux() {
        assert_eq!(core::mem::size_of::<PerfEventMmapPage>(), 1088);
        assert_eq!(PERF_MMAP_DATA_HEAD_OFFSET, 1024);
        assert_eq!(PERF_MMAP_AUX_HEAD_OFFSET, 1056);
        assert_eq!(core::mem::size_of::<PerfEventQueryBpf>(), 8);
        assert_eq!(PERF_EVENT_IOC_PERIOD, 0x4008_2404);
        assert_eq!(PERF_EVENT_IOC_MODIFY_ATTRIBUTES, 0x4008_240b);
        assert_eq!(PERF_RECORD_AUX, 11);
        assert_eq!(PERF_RECORD_CALLCHAIN_DEFERRED, 22);
        assert_eq!(PERF_AUX_FLAG_COLLISION, 8);
        assert_eq!(PERF_RECORD_MISC_FORK_EXEC, PERF_RECORD_MISC_COMM_EXEC);
    }

    #[test]
    fn record_codecs_preserve_linux_layout() {
        let mut sample = [0xff; 40];
        assert_eq!(
            encode_sample_record(
                &mut sample,
                PERF_SAMPLE_IP | PERF_SAMPLE_TIME | PERF_SAMPLE_CPU | PERF_SAMPLE_PERIOD,
                1,
                true,
                2,
                3,
                4,
            ),
            40
        );
        assert_eq!(
            u32::from_ne_bytes(sample[..4].try_into().unwrap()),
            PERF_RECORD_SAMPLE
        );
        assert_eq!(
            u16::from_ne_bytes(sample[4..6].try_into().unwrap()),
            PERF_RECORD_MISC_USER
        );
        assert_eq!(u16::from_ne_bytes(sample[6..8].try_into().unwrap()), 40);
        assert_eq!(u64::from_ne_bytes(sample[8..16].try_into().unwrap()), 1);
        assert_eq!(u64::from_ne_bytes(sample[16..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_ne_bytes(sample[24..28].try_into().unwrap()), 3);
        assert_eq!(&sample[28..32], &[0; 4]);
        assert_eq!(u64::from_ne_bytes(sample[32..40].try_into().unwrap()), 4);

        let mut lost = [0; 24];
        encode_lost_record(&mut lost, 7, 8);
        assert_eq!(
            u32::from_ne_bytes(lost[..4].try_into().unwrap()),
            PERF_RECORD_LOST
        );
        assert_eq!(u16::from_ne_bytes(lost[4..6].try_into().unwrap()), 0);
        assert_eq!(u16::from_ne_bytes(lost[6..8].try_into().unwrap()), 24);
        assert_eq!(u64::from_ne_bytes(lost[8..16].try_into().unwrap()), 7);
        assert_eq!(u64::from_ne_bytes(lost[16..24].try_into().unwrap()), 8);
    }

    #[test]
    fn each_supported_sample_field_has_one_word_record() {
        for sample_type in [
            PERF_SAMPLE_IP,
            PERF_SAMPLE_TIME,
            PERF_SAMPLE_CPU,
            PERF_SAMPLE_PERIOD,
        ] {
            let mut sample = [0xff; 40];
            assert_eq!(
                encode_sample_record(&mut sample, sample_type, 0, false, 0, 0, 0),
                16
            );
            assert_eq!(u16::from_ne_bytes(sample[6..8].try_into().unwrap()), 16);
        }
    }
    #[test]
    fn sample_record() {
        let mut x = i();
        x.attr.sample_period = 1;
        x.attr.wakeup_events = 1;
        x.attr.sample_type = PERF_SAMPLE_IP | PERF_SAMPLE_RAW;
        let p = plan(
            x,
            PerfSnapshot::default(),
            FeatureSet {
                hardware: true,
                sampling: true,
                sample_type: PERF_SAMPLE_IP | PERF_SAMPLE_RAW,
                ..FeatureSet::default()
            },
        )
        .unwrap();
        assert_eq!(p.sample.unwrap().fixed_words, 1);
    }

    #[test]
    fn sampling_admission_is_planned_once() {
        let mut x = i();
        x.attr.sample_period = 4096;
        x.attr.sample_type = PERF_SAMPLE_IP;
        x.attr.wakeup_events = 1;
        let features = FeatureSet {
            hardware: true,
            sampling: true,
            sample_type: PERF_SAMPLE_IP,
            read_format: PERF_FORMAT_IMPLEMENTED,
            min_sample_period: 4096,
            max_wakeup_events: 1,
            sampling_read_format: PERF_FORMAT_IMPLEMENTED & !PERF_FORMAT_GROUP,
            sampling_requires_zero_config1: true,
            sampling_hardware_only: true,
            ..FeatureSet::default()
        };
        assert_eq!(
            plan(x, PerfSnapshot::default(), features)
                .unwrap()
                .sample
                .unwrap()
                .period,
            4096
        );
        x.attr.sample_type = PERF_SAMPLE_RAW;
        assert_eq!(
            plan(x, PerfSnapshot::default(), features),
            Err(Reject::UnsupportedSampleType)
        );
        x.attr.sample_type = PERF_SAMPLE_IP;
        x.attr.wakeup_events = 2;
        assert_eq!(
            plan(x, PerfSnapshot::default(), features),
            Err(Reject::InvalidWakeup)
        );
        x.attr.wakeup_events = 1;
        x.attr.read_format = PERF_FORMAT_GROUP;
        assert_eq!(
            plan(x, PerfSnapshot::default(), features),
            Err(Reject::InvalidSamplingMode)
        );
        x.attr.read_format = 0;
        x.attr.config1 = 1;
        assert_eq!(
            plan(x, PerfSnapshot::default(), features),
            Err(Reject::InvalidSamplingMode)
        );
        x.attr.config1 = 0;
        x.attr.sample_type = 0;
        assert_eq!(
            plan(x, PerfSnapshot::default(), features),
            Err(Reject::InvalidSamplingMode)
        );
    }

    #[test]
    fn implemented_attr_flags_are_reflected_in_the_plan() {
        let mut x = i();
        x.attr.flags = ATTR_DISABLED | ATTR_EXCLUDE_USER | ATTR_PINNED | ATTR_EXCLUSIVE;
        let p = plan(
            x,
            PerfSnapshot::default(),
            FeatureSet {
                hardware: true,
                attr_flags: ATTR_IMPLEMENTED,
                ..FeatureSet::default()
            },
        )
        .unwrap();
        assert!(p.disabled);
        assert!(p.exclude_user);
        assert!(!p.exclude_kernel);
    }

    #[test]
    fn placement_flags_are_accepted_only_when_the_adapter_advertises_them() {
        let mut x = i();
        x.attr.flags = ATTR_PINNED | ATTR_EXCLUSIVE;
        assert!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    attr_flags: ATTR_IMPLEMENTED,
                    ..FeatureSet::default()
                },
            )
            .is_ok()
        );
        assert_eq!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    attr_flags: ATTR_IMPLEMENTED & !ATTR_EXCLUSIVE,
                    ..FeatureSet::default()
                },
            ),
            Err(Reject::UnsupportedAttrFlags),
        );
    }

    #[test]
    fn adapter_rejects_implemented_attr_flags_it_cannot_honor() {
        let mut x = i();
        x.attr.flags = ATTR_EXCLUDE_KERNEL;
        assert_eq!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    ..FeatureSet::default()
                },
            ),
            Err(Reject::UnsupportedAttrFlags),
        );
    }

    #[test]
    fn known_but_unimplemented_attr_flags_are_rejected() {
        for flag in [
            ATTR_INHERIT,
            ATTR_PINNED,
            ATTR_EXCLUSIVE,
            ATTR_FREQ,
            ATTR_ENABLE_ON_EXEC,
        ] {
            let mut x = i();
            x.attr.flags = flag;
            assert_eq!(
                plan(
                    x,
                    PerfSnapshot::default(),
                    FeatureSet {
                        hardware: true,
                        ..FeatureSet::default()
                    },
                ),
                Err(Reject::UnsupportedAttrFlags),
                "flag {flag:#x}",
            );
        }
    }

    #[test]
    fn read_format_requires_adapter_support_and_accepts_lost_when_enabled() {
        let mut x = i();
        x.attr.read_format = PERF_FORMAT_ID;
        assert_eq!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    ..FeatureSet::default()
                },
            ),
            Err(Reject::UnsupportedReadFormat),
        );

        x.attr.read_format = PERF_FORMAT_ID;
        assert!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    read_format: PERF_FORMAT_ID,
                    ..FeatureSet::default()
                },
            )
            .is_ok()
        );

        x.attr.read_format = PERF_FORMAT_LOST;
        assert!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    read_format: PERF_FORMAT_ALL,
                    ..FeatureSet::default()
                },
            )
            .is_ok()
        );
    }

    #[test]
    fn open_flags_follow_advertised_features_and_schema_capabilities() {
        let mut x = i();
        x.open_flags = PERF_OPEN_FLAGS_IMPLEMENTED;
        let p = plan(
            x,
            PerfSnapshot::default(),
            FeatureSet {
                hardware: true,
                open_flags: PERF_OPEN_FLAGS_IMPLEMENTED,
                ..FeatureSet::default()
            },
        )
        .unwrap();
        assert!(p.close_on_exec);

        x.open_flags = PERF_FLAG_FD_OUTPUT;
        assert_eq!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    open_flags: PERF_FLAG_FD_CLOEXEC,
                    ..FeatureSet::default()
                },
            ),
            Err(Reject::UnsupportedOpenFlags),
        );

        let mut x = modern_input();
        x.target = PerfOpenTarget {
            target: PerfTarget::Cgroup { fd: 7, cpu: 0 },
            group_fd: -1,
            output_fd: 9,
            open_flags: PERF_FLAG_FD_NO_GROUP
                | PERF_FLAG_FD_OUTPUT
                | PERF_FLAG_PID_CGROUP
                | PERF_FLAG_FD_CLOEXEC,
        };
        assert!(plan_attr(x, LINUX_V71_SCHEMA).is_ok());
        assert_eq!(
            plan_attr(
                x,
                PerfCapabilities {
                    open_flags: LINUX_V71_SCHEMA.open_flags & !PERF_FLAG_PID_CGROUP,
                    ..LINUX_V71_SCHEMA
                },
            ),
            Err(AttrReject::UnsupportedOpenFlags),
        );
        assert_eq!(
            plan_attr(
                x,
                PerfCapabilities {
                    supports_cgroup: false,
                    ..LINUX_V71_SCHEMA
                },
            ),
            Err(AttrReject::UnsupportedTarget),
        );
        assert_eq!(
            plan_attr(
                x,
                PerfCapabilities {
                    supports_output: false,
                    ..LINUX_V71_SCHEMA
                },
            ),
            Err(AttrReject::UnsupportedTarget),
        );
    }

    #[test]
    fn software_accounting_events_are_planned() {
        let mut x = i();
        x.attr.event_type = PERF_TYPE_SOFTWARE;
        x.attr.config = PERF_COUNT_SW_PAGE_FAULTS;
        let features = FeatureSet {
            software: true,
            ..FeatureSet::default()
        };
        assert_eq!(
            plan(x, PerfSnapshot::default(), features).unwrap().event,
            Event::SoftwarePageFaults
        );
        x.attr.config = PERF_COUNT_SW_CONTEXT_SWITCHES;
        assert_eq!(
            plan(x, PerfSnapshot::default(), features).unwrap().event,
            Event::SoftwareContextSwitches
        );
        x.attr.config = PERF_COUNT_SW_CPU_MIGRATIONS;
        assert_eq!(
            plan(x, PerfSnapshot::default(), features).unwrap().event,
            Event::SoftwareCpuMigrations
        );
        x.attr.config = PERF_COUNT_SW_PAGE_FAULTS_MIN;
        assert_eq!(
            plan(x, PerfSnapshot::default(), features).unwrap().event,
            Event::SoftwarePageFaultsMin
        );
        x.attr.config = PERF_COUNT_SW_PAGE_FAULTS_MAJ;
        assert_eq!(
            plan(x, PerfSnapshot::default(), features).unwrap().event,
            Event::SoftwarePageFaultsMaj
        );
    }

    #[test]
    fn tracepoint_requires_adapter_source() {
        let mut x = i();
        x.attr.event_type = PERF_TYPE_TRACEPOINT;
        x.attr.config = 1;
        assert_eq!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    tracepoint: true,
                    ..FeatureSet::default()
                },
            )
            .unwrap()
            .event,
            Event::Tracepoint(1)
        );
    }

    #[test]
    fn known_configs_are_distinct_from_malformed_configs() {
        let mut x = i();
        x.attr.config = PERF_COUNT_HW_CACHE_REFERENCES;
        assert_eq!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    ..FeatureSet::default()
                }
            ),
            Ok(PerfOpenPlan {
                event: Event::HardwareCacheReferences,
                disabled: false,
                exclude_user: false,
                exclude_kernel: false,
                close_on_exec: false,
                lifecycle: PerfLifecycle::default(),
                sample: None,
                read: ReadPlan {
                    group: false,
                    time_enabled: false,
                    time_running: false,
                    id: false,
                    lost: false,
                },
            })
        );
        x.attr.config = u64::MAX;
        assert_eq!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    ..FeatureSet::default()
                }
            ),
            Err(Reject::InvalidEvent)
        );
        x.attr.event_type = PERF_TYPE_BREAKPOINT;
        assert_eq!(
            plan(
                x,
                PerfSnapshot::default(),
                FeatureSet {
                    hardware: true,
                    ..FeatureSet::default()
                }
            ),
            Err(Reject::UnsupportedEvent)
        );
    }

    fn modern_input() -> PerfAttrInput<'static> {
        PerfAttrInput {
            attr: PerfEventAttr {
                event_type: PERF_TYPE_HARDWARE,
                size: PERF_ATTR_SIZE_VER9,
                config: PERF_COUNT_HW_CPU_CYCLES,
                ..PerfEventAttr::default()
            },
            supplied_size: PERF_ATTR_SIZE_VER9,
            extra_tail: &[],
            target: PerfOpenTarget {
                target: PerfTarget::Task { pid: 0, cpu: -1 },
                group_fd: -1,
                output_fd: -1,
                open_flags: 0,
            },
        }
    }

    #[test]
    fn schema_observes_every_prefix_boundary_and_zeros_future_fields() {
        for size in PERF_ATTR_SIZES {
            let mut x = modern_input();
            x.supplied_size = size;
            x.attr.config4 = 0xfeed;
            let p = plan_attr(x, LINUX_V71_SCHEMA).unwrap();
            assert_eq!(
                p.extensions.config4,
                if size == PERF_ATTR_SIZE_VER9 {
                    0xfeed
                } else {
                    0
                }
            );
        }
        let mut x = modern_input();
        x.supplied_size = PERF_ATTR_SIZE_VER9 + 8;
        x.extra_tail = &[0; 8];
        assert!(
            plan_attr(
                x,
                PerfCapabilities {
                    max_attr_size: PERF_ATTR_SIZE_VER9 + 8,
                    ..LINUX_V71_SCHEMA
                }
            )
            .is_ok()
        );
        x.extra_tail = &[1; 8];
        assert_eq!(
            plan_attr(
                x,
                PerfCapabilities {
                    max_attr_size: PERF_ATTR_SIZE_VER9 + 8,
                    ..LINUX_V71_SCHEMA
                }
            ),
            Err(AttrReject::NonZeroTail)
        );
    }

    #[test]
    fn schema_never_admits_a_single_missing_capability_bit() {
        for shift in 0..40 {
            let bit = 1u64 << shift;
            let mut x = modern_input();
            x.attr.flags = bit;
            let caps = PerfCapabilities {
                attr_flags: LINUX_V71_SCHEMA.attr_flags & !bit,
                ..LINUX_V71_SCHEMA
            };
            assert_eq!(
                plan_attr(x, caps),
                Err(AttrReject::UnsupportedAttrFlags),
                "attr bit {bit:#x}"
            );
        }
        for shift in 0..25 {
            let bit = 1u64 << shift;
            let mut x = modern_input();
            x.attr.sample_period = 1;
            x.attr.wakeup_events = 1;
            x.attr.sample_type = bit;
            let caps = PerfCapabilities {
                sample_type: LINUX_V71_SCHEMA.sample_type & !bit,
                ..LINUX_V71_SCHEMA
            };
            assert_eq!(
                plan_attr(x, caps),
                Err(AttrReject::UnsupportedSampleType),
                "sample bit {bit:#x}"
            );
        }
        for shift in 0..5 {
            let bit = 1u64 << shift;
            let mut x = modern_input();
            x.attr.read_format = bit;
            let caps = PerfCapabilities {
                read_format: LINUX_V71_SCHEMA.read_format & !bit,
                ..LINUX_V71_SCHEMA
            };
            assert_eq!(
                plan_attr(x, caps),
                Err(AttrReject::UnsupportedReadFormat),
                "read bit {bit:#x}"
            );
        }
    }

    #[test]
    fn sampling_accepts_zero_event_wakeup_threshold() {
        let mut x = modern_input();
        x.attr.event_type = PERF_TYPE_TRACEPOINT;
        x.attr.config = 4;
        x.attr.sample_period = 1;
        x.attr.sample_type = PERF_SAMPLE_TIME | PERF_SAMPLE_RAW;
        x.attr.wakeup_events = 0;
        let p = plan_attr(x, LINUX_V71_SCHEMA).unwrap();
        assert_eq!(p.wakeup, Wakeup::Events(0));
        assert_eq!(p.period, Some(SamplePeriod::Period(1)));
    }

    #[test]
    fn schema_period_wakeup_target_and_codecs_are_exact() {
        let mut x = modern_input();
        x.attr.flags = ATTR_FREQ | ATTR_WATERMARK;
        x.attr.sample_period = 99;
        x.attr.wakeup_events = 64;
        let p = plan_attr(x, LINUX_V71_SCHEMA).unwrap();
        assert_eq!(p.period, Some(SamplePeriod::Frequency(99)));
        assert_eq!(p.wakeup, Wakeup::Watermark(64));
        let mut x = modern_input();
        x.target.group_fd = 7;
        assert_eq!(
            plan_attr(
                x,
                PerfCapabilities {
                    supports_group: false,
                    ..LINUX_V71_SCHEMA
                }
            ),
            Err(AttrReject::UnsupportedTarget)
        );
        x.target.open_flags = PERF_FLAG_FD_OUTPUT;
        x.target.output_fd = 9;
        assert_eq!(
            plan_attr(
                x,
                PerfCapabilities {
                    supports_output: false,
                    ..LINUX_V71_SCHEMA
                }
            ),
            Err(AttrReject::UnsupportedTarget)
        );
        let mut data = [0u8; 32];
        let aux = AuxRecord {
            offset: 8,
            size: 16,
            flags: PERF_AUX_FLAG_OVERWRITE,
        };
        assert_eq!(encode_aux_record(&mut data, aux), Ok(32));
        assert_eq!(decode_aux_record(&data), Ok(aux));
        assert_eq!(
            checked_read_size(
                ReadPlan {
                    group: true,
                    time_enabled: true,
                    time_running: true,
                    id: false,
                    lost: false
                },
                2
            ),
            Ok(40)
        );
        assert!(
            MmapRingLayout {
                data_offset: 4096,
                data_size: 4096,
                aux_offset: 8192,
                aux_size: 4096
            }
            .validate()
            .is_ok()
        );
        assert_eq!(
            MmapRingLayout {
                data_offset: 1,
                data_size: 4096,
                aux_offset: 0,
                aux_size: 0
            }
            .validate(),
            Err(CodecError::Misaligned)
        );
    }

    #[test]
    fn malformed_input_precedes_capability_rejection() {
        let mut x = modern_input();
        x.attr.flags = 1 << 63;
        x.supplied_size = PERF_ATTR_SIZE_VER0 - 1;
        assert_eq!(
            plan_attr(x, PerfCapabilities::default()),
            Err(AttrReject::SizeTooSmall)
        );
        x.supplied_size = PERF_ATTR_SIZE_VER0;
        assert_eq!(
            plan_attr(x, LINUX_V71_SCHEMA),
            Err(AttrReject::UnknownAttrFlags)
        );
        x.attr.flags = 0;
        x.target.open_flags = 1 << 63;
        assert_eq!(
            plan_attr(x, LINUX_V71_SCHEMA),
            Err(AttrReject::UnknownOpenFlags)
        );
    }
}
