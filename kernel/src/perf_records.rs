//! Linux perf non-sample record encoding.
//!
//! Lifecycle code supplies stable process/MM snapshots; this module owns the
//! exact native-endian bodies, eight-byte padding, and SAMPLE_ID_ALL trailer.

use thekernel_linux_perf::{
    PERF_RECORD_COMM, PERF_RECORD_EXIT, PERF_RECORD_FORK, PERF_RECORD_MISC_USER, PERF_RECORD_MMAP,
    PERF_RECORD_MMAP2, PERF_RECORD_READ, PERF_RECORD_SWITCH, PERF_RECORD_SWITCH_CPU_WIDE,
    PerfRecordHeader,
};

/// Stable VMA metadata captured before `mmap(2)` consumes its file lease and
/// published only after the mapping transaction commits.
pub(crate) struct MmapInfo<'a> {
    pub filename: &'a [u8],
    pub major: u32,
    pub minor: u32,
    pub ino: u64,
    pub ino_generation: u64,
    pub prot: u32,
    pub flags: u32,
    pub executable: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct SampleId {
    pub sample_type: u64,
    pub pid: u32,
    pub tid: u32,
    pub time: u64,
    pub id: u64,
    pub stream_id: u64,
    pub cpu: u32,
}

const SAMPLE_IDENTIFIER: u64 = 1 << 16;
const SAMPLE_IP: u64 = 1 << 0;
const SAMPLE_TID: u64 = 1 << 1;
const SAMPLE_TIME: u64 = 1 << 2;
const SAMPLE_ADDR: u64 = 1 << 3;
const SAMPLE_ID: u64 = 1 << 6;
const SAMPLE_STREAM_ID: u64 = 1 << 9;
const SAMPLE_CPU: u64 = 1 << 7;

fn put(out: &mut [u8], cursor: &mut usize, bytes: &[u8]) -> Option<()> {
    let end = cursor.checked_add(bytes.len())?;
    out.get_mut(*cursor..end)?.copy_from_slice(bytes);
    *cursor = end;
    Some(())
}
fn u32(out: &mut [u8], cursor: &mut usize, value: u32) -> Option<()> {
    put(out, cursor, &value.to_ne_bytes())
}
fn u64(out: &mut [u8], cursor: &mut usize, value: u64) -> Option<()> {
    put(out, cursor, &value.to_ne_bytes())
}

fn pad(cursor: &mut usize) {
    *cursor = (*cursor + 7) & !7;
}

fn sample_id_len(sample: Option<SampleId>) -> Option<usize> {
    let Some(sample) = sample else { return Some(0) };
    let mut len = 0usize;
    if sample.sample_type & SAMPLE_TID != 0 {
        len = len.checked_add(8)?;
    }
    if sample.sample_type & SAMPLE_TIME != 0 {
        len = len.checked_add(8)?;
    }
    if sample.sample_type & SAMPLE_ID != 0 {
        len = len.checked_add(8)?;
    }
    if sample.sample_type & SAMPLE_STREAM_ID != 0 {
        len = len.checked_add(8)?;
    }
    if sample.sample_type & SAMPLE_CPU != 0 {
        len = len.checked_add(8)?;
    }
    if sample.sample_type & SAMPLE_IDENTIFIER != 0 {
        len = len.checked_add(8)?;
    }
    Some(len)
}

/// Appends the Linux SAMPLE_ID_ALL trailer in sample-field order. Fields that
/// are not legal in a non-sample trailer are intentionally ignored.
fn sample_id(out: &mut [u8], cursor: &mut usize, sample: SampleId) -> Option<()> {
    if sample.sample_type & SAMPLE_TID != 0 {
        u32(out, cursor, sample.pid)?;
        u32(out, cursor, sample.tid)?;
    }
    if sample.sample_type & SAMPLE_TIME != 0 {
        u64(out, cursor, sample.time)?;
    }
    if sample.sample_type & SAMPLE_ID != 0 {
        u64(out, cursor, sample.id)?;
    }
    if sample.sample_type & SAMPLE_STREAM_ID != 0 {
        u64(out, cursor, sample.stream_id)?;
    }
    if sample.sample_type & SAMPLE_CPU != 0 {
        u32(out, cursor, sample.cpu)?;
        u32(out, cursor, 0)?;
    }
    if sample.sample_type & SAMPLE_IDENTIFIER != 0 {
        u64(out, cursor, sample.id)?;
    }
    let _ = SAMPLE_IP | SAMPLE_ADDR; // not part of sample_id_all trailers
    Some(())
}

fn finish(
    out: &mut [u8],
    kind: u32,
    misc: u16,
    body: usize,
    sample: Option<SampleId>,
) -> Option<usize> {
    let mut end = body;
    if let Some(sample) = sample {
        sample_id(out, &mut end, sample)?;
    }
    let unpadded_end = end;
    pad(&mut end);
    if end > out.len() {
        return None;
    }
    out.get_mut(unpadded_end..end)?.fill(0);
    let size = u16::try_from(end).ok()?;
    let mut header = [0u8; 8];
    PerfRecordHeader::new(kind, misc, size).encode(&mut header);
    out.get_mut(..8)?.copy_from_slice(&header);
    Some(end)
}

pub(crate) fn comm(
    out: &mut [u8],
    misc: u16,
    pid: u32,
    tid: u32,
    name: &[u8],
    sample: Option<SampleId>,
) -> Option<usize> {
    let mut at = 8;
    u32(out, &mut at, pid)?;
    u32(out, &mut at, tid)?;
    put(out, &mut at, name)?;
    put(out, &mut at, &[0])?;
    pad(&mut at);
    finish(out, PERF_RECORD_COMM, misc, at, sample)
}
pub(crate) fn fork_exit(
    out: &mut [u8],
    kind: u32,
    pid: u32,
    ppid: u32,
    tid: u32,
    ptid: u32,
    time: u64,
    sample: Option<SampleId>,
) -> Option<usize> {
    if kind != PERF_RECORD_FORK && kind != PERF_RECORD_EXIT {
        return None;
    }
    let mut at = 8;
    u32(out, &mut at, pid)?;
    u32(out, &mut at, ppid)?;
    u32(out, &mut at, tid)?;
    u32(out, &mut at, ptid)?;
    u64(out, &mut at, time)?;
    finish(out, kind, 0, at, sample)
}
pub(crate) fn switch(
    out: &mut [u8],
    misc: u16,
    next_pid: Option<(u32, u32)>,
    sample: Option<SampleId>,
) -> Option<usize> {
    let mut at = 8;
    let kind = if let Some((pid, tid)) = next_pid {
        u32(out, &mut at, pid)?;
        u32(out, &mut at, tid)?;
        PERF_RECORD_SWITCH_CPU_WIDE
    } else {
        PERF_RECORD_SWITCH
    };
    finish(out, kind, misc, at, sample)
}
pub(crate) fn mmap(
    out: &mut [u8],
    mmap2: bool,
    pid: u32,
    tid: u32,
    addr: u64,
    len: u64,
    pgoff: u64,
    info: &MmapInfo<'_>,
    sample: Option<SampleId>,
) -> Option<usize> {
    let mut at = 8;
    u32(out, &mut at, pid)?;
    u32(out, &mut at, tid)?;
    u64(out, &mut at, addr)?;
    u64(out, &mut at, len)?;
    u64(out, &mut at, pgoff)?;
    if mmap2 {
        u32(out, &mut at, info.major)?;
        u32(out, &mut at, info.minor)?;
        u64(out, &mut at, info.ino)?;
        u64(out, &mut at, info.ino_generation)?;
        u32(out, &mut at, info.prot)?;
        u32(out, &mut at, info.flags)?;
    }
    put(out, &mut at, info.filename)?;
    put(out, &mut at, &[0])?;
    pad(&mut at);
    finish(
        out,
        if mmap2 {
            PERF_RECORD_MMAP2
        } else {
            PERF_RECORD_MMAP
        },
        PERF_RECORD_MISC_USER,
        at,
        sample,
    )
}

/// Exact bounded allocation requirement for an MMAP/MMAP2 lifecycle record.
/// This keeps pathname delivery lossless for ordinary task-context producers:
/// callers can allocate once before encoding instead of silently discarding a
/// record because a fixed temporary happened to be too small.
pub(crate) fn mmap_record_len(
    mmap2: bool,
    filename_len: usize,
    sample: Option<SampleId>,
) -> Option<usize> {
    let mut len = 8usize // header
        .checked_add(8)? // pid/tid
        .checked_add(24)?; // addr/len/pgoff
    if mmap2 {
        len = len.checked_add(32)?;
    }
    len = len.checked_add(filename_len)?.checked_add(1)?;
    pad(&mut len);
    len = len.checked_add(sample_id_len(sample)?)?;
    pad(&mut len);
    (len <= u16::MAX as usize).then_some(len)
}
pub(crate) fn read(
    out: &mut [u8],
    pid: u32,
    tid: u32,
    read_body: &[u8],
    sample: Option<SampleId>,
) -> Option<usize> {
    let mut at = 8;
    u32(out, &mut at, pid)?;
    u32(out, &mut at, tid)?;
    put(out, &mut at, read_body)?;
    pad(&mut at);
    finish(out, PERF_RECORD_READ, 0, at, sample)
}
