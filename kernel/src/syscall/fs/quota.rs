//! Linux quota-control ABI backed by each VFS mount root.
use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{DeviceId, FsPathBuf, Location, NodeType};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{AT_FDCWD, CAP_SYS_ADMIN};
use tk_linux_cred::Kgid;
use tk_linux_usercopy::{
    UserMemory, UserMemoryContext, vm_load, vm_load_until_nul, vm_write_slice,
};

use super::ctl::validate_pathname;
use crate::{
    file::{
        Directory, File, ResolveAtResult, get_file_like, permission::VfsSecurityContext,
        resolve_at_with_security,
    },
    mm::map_usercopy_error,
    task::AsThread,
};

const SUBCMDMASK: u32 = 0xff;
const SUBCMDSHIFT: u32 = 8;
const Q_SYNC: u32 = 0x800001;
const Q_QUOTAON: u32 = 0x800002;
const Q_QUOTAOFF: u32 = 0x800003;
const Q_GETFMT: u32 = 0x800004;
const Q_GETINFO: u32 = 0x800005;
const Q_SETINFO: u32 = 0x800006;
const Q_GETQUOTA: u32 = 0x800007;
const Q_SETQUOTA: u32 = 0x800008;
const Q_GETNEXTQUOTA: u32 = 0x800009;
// The XFS quota family is `XQM_CMD(x)`, i.e. `('X' << 8) + x`
// (include/uapi/linux/dqblk_xfs.h:27-43), so the shifted selector is 0x5801
// through 0x5809 rather than the 0x8000xx range of the Q_* family.
const Q_XQUOTAON: u32 = 0x5801;
const Q_XQUOTAOFF: u32 = 0x5802;
const Q_XGETQUOTA: u32 = 0x5803;
const Q_XSETQLIM: u32 = 0x5804;
const Q_XGETQSTAT: u32 = 0x5805;
const Q_XQUOTARM: u32 = 0x5806;
const Q_XQUOTASYNC: u32 = 0x5807;
const Q_XGETQSTATV: u32 = 0x5808;
const Q_XGETNEXTQUOTA: u32 = 0x5809;
// `fs_disk_quota.d_fieldmask` (include/uapi/linux/dqblk_xfs.h:56-104).
const FS_DQ_ISOFT: u32 = 1 << 0;
const FS_DQ_IHARD: u32 = 1 << 1;
const FS_DQ_BSOFT: u32 = 1 << 2;
const FS_DQ_BHARD: u32 = 1 << 3;
const FS_DQ_RTBSOFT: u32 = 1 << 4;
const FS_DQ_RTBHARD: u32 = 1 << 5;
const FS_DQ_BTIMER: u32 = 1 << 6;
const FS_DQ_ITIMER: u32 = 1 << 7;
const FS_DQ_RTBTIMER: u32 = 1 << 8;
const FS_DQ_BWARNS: u32 = 1 << 9;
const FS_DQ_IWARNS: u32 = 1 << 10;
const FS_DQ_RTBWARNS: u32 = 1 << 11;
const FS_DQ_BCOUNT: u32 = 1 << 12;
const FS_DQ_ICOUNT: u32 = 1 << 13;
const FS_DQ_RTBCOUNT: u32 = 1 << 14;
const FS_DQ_BIGTIME: u32 = 1 << 15;
// `fs_quota_stat.qs_flags` (include/uapi/linux/dqblk_xfs.h:137-145).
const FS_QUOTA_UDQ_ACCT: u16 = 1 << 0;
const FS_QUOTA_UDQ_ENFD: u16 = 1 << 1;
const FS_QUOTA_GDQ_ACCT: u16 = 1 << 2;
const FS_QUOTA_GDQ_ENFD: u16 = 1 << 3;
const FS_QUOTA_PDQ_ACCT: u16 = 1 << 4;
const FS_QUOTA_PDQ_ENFD: u16 = 1 << 5;
// `fs_disk_quota.d_flags` / `d_version` and the two stat versions
// (include/uapi/linux/dqblk_xfs.h:52, :147-157).
const FS_USER_QUOTA: i8 = 1 << 0;
const FS_PROJ_QUOTA: i8 = 1 << 1;
const FS_GROUP_QUOTA: i8 = 1 << 2;
const FS_DQUOT_VERSION: i8 = 1;
const FS_QSTAT_VERSION: i8 = 1;
// `quota_btobb()`/`quota_bbtob()`: fs_disk_quota counts 512-byte basic blocks
// while the VFS quota structures count bytes (fs/quota/quota.c:522-532).
const XFS_BB_SHIFT: u32 = 9;
// `QIF_DQBLKSIZE_BITS` / `QIF_DQBLKSIZE` (include/uapi/linux/quota.h:84-85):
// `struct if_dqblk` counts 1024-byte quota blocks where `qc_dqblk` and the
// quota file both count bytes.
const QIF_DQBLKSIZE_BITS: u32 = 10;
const QIF_DQBLKSIZE: u64 = 1 << QIF_DQBLKSIZE_BITS;
const QFMT_VFS_V1: u32 = 4;
const QFMT_VFS_OLD: u32 = 1;
const QIF_BLIMITS: u32 = 1;
const QIF_SPACE: u32 = 2;
const QIF_ILIMITS: u32 = 4;
const QIF_INODES: u32 = 8;
const QIF_BTIME: u32 = 16;
const QIF_ITIME: u32 = 32;
const QIF_BGRACE: u32 = 1;
const QIF_IGRACE: u32 = 2;
const QIF_FLAGS: u32 = 4;
// The kernel-internal `QC_*` field specifiers both wire protocols translate
// into (include/linux/quota.h:369-389).  These are the bits `do_set_dqblk()`
// and `dquot_set_dqinfo()` actually test.
const QC_INO_SOFT: u32 = 1 << 0;
const QC_INO_HARD: u32 = 1 << 1;
const QC_SPC_SOFT: u32 = 1 << 2;
const QC_SPC_HARD: u32 = 1 << 3;
const QC_RT_SPC_SOFT: u32 = 1 << 4;
const QC_RT_SPC_HARD: u32 = 1 << 5;
const QC_SPC_TIMER: u32 = 1 << 6;
const QC_INO_TIMER: u32 = 1 << 7;
const QC_RT_SPC_TIMER: u32 = 1 << 8;
const QC_SPC_WARNS: u32 = 1 << 9;
const QC_INO_WARNS: u32 = 1 << 10;
const QC_RT_SPC_WARNS: u32 = 1 << 11;
const QC_SPACE: u32 = 1 << 12;
const QC_INO_COUNT: u32 = 1 << 13;
const QC_RT_SPACE: u32 = 1 << 14;
const QC_FLAGS: u32 = 1 << 15;
const QC_WARNS_MASK: u32 = QC_SPC_WARNS | QC_INO_WARNS | QC_RT_SPC_WARNS;
/// `VFS_QC_MASK` (fs/quota/dquot.c:2740-2743): every selector the generic
/// dquot provider accepts.
const VFS_QC_MASK: u32 = QC_SPACE
    | QC_SPC_SOFT
    | QC_SPC_HARD
    | QC_INO_COUNT
    | QC_INO_SOFT
    | QC_INO_HARD
    | QC_SPC_TIMER
    | QC_INO_TIMER;
/// `FS_DQ_WARNS_MASK` / `FS_DQ_TIMER_MASK`
/// (include/uapi/linux/dqblk_xfs.h:108-113): the two groups
/// `quota_setxquota()` diverts to `->set_info()` for the superuser dquot.
const FS_DQ_TIMER_MASK: u32 = FS_DQ_BTIMER | FS_DQ_ITIMER | FS_DQ_RTBTIMER;
const FS_DQ_WARNS_MASK: u32 = FS_DQ_BWARNS | FS_DQ_IWARNS | FS_DQ_RTBWARNS;
/// `copy_from_xfs_dqblk()`'s selector table, in the order that function tests
/// them (fs/quota/quota.c:571-597).  `FS_DQ_BIGTIME` is deliberately absent:
/// it is a timer-width flag, not a field selector, so it selects nothing.
const XFS_QC_SELECTORS: [(u32, u32); 15] = [
    (FS_DQ_ISOFT, QC_INO_SOFT),
    (FS_DQ_IHARD, QC_INO_HARD),
    (FS_DQ_BSOFT, QC_SPC_SOFT),
    (FS_DQ_BHARD, QC_SPC_HARD),
    (FS_DQ_RTBSOFT, QC_RT_SPC_SOFT),
    (FS_DQ_RTBHARD, QC_RT_SPC_HARD),
    (FS_DQ_BTIMER, QC_SPC_TIMER),
    (FS_DQ_ITIMER, QC_INO_TIMER),
    (FS_DQ_RTBTIMER, QC_RT_SPC_TIMER),
    (FS_DQ_BWARNS, QC_SPC_WARNS),
    (FS_DQ_IWARNS, QC_INO_WARNS),
    (FS_DQ_RTBWARNS, QC_RT_SPC_WARNS),
    (FS_DQ_BCOUNT, QC_SPACE),
    (FS_DQ_ICOUNT, QC_INO_COUNT),
    (FS_DQ_RTBCOUNT, QC_RT_SPACE),
];
const DQBLK_VALID_MASK: u32 =
    QIF_BLIMITS | QIF_SPACE | QIF_ILIMITS | QIF_INODES | QIF_BTIME | QIF_ITIME;
const DQINFO_VALID_MASK: u32 = QIF_BGRACE | QIF_IGRACE | QIF_FLAGS;
// `DQF_SETINFO_MASK` is DQF_ROOT_SQUASH: the only quota-info flag a
// Q_SETINFO caller may set.  DQF_SYS_FILE is reported by Q_GETINFO but is
// owned by the kernel.
const DQF_ROOT_SQUASH: u32 = 1;
const DQF_SETINFO_MASK: u32 = DQF_ROOT_SQUASH;
// Linux's v2 on-disk quota format.  All fields are explicitly little endian:
// quota files are data files, not native-endian kernel snapshots.
const V2_VERSION: u32 = 1;
const QUOTA_BLOCK: usize = 1024;
// A quota file is administrative metadata, not an unbounded userspace data
// stream.  Keep activation memory bounded; the sparse v2 tree itself is also
// limited by this cap.
const MAX_QUOTA_FILE_BYTES: usize = 64 * 1024 * 1024;

#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct IfDqblk {
    bhardlimit: u64,
    bsoftlimit: u64,
    curspace: u64,
    ihardlimit: u64,
    isoftlimit: u64,
    curinodes: u64,
    btime: u64,
    itime: u64,
    valid: u32,
    _pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct IfDqinfo {
    bgrace: u64,
    igrace: u64,
    flags: u32,
    valid: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct IfNextDqblk {
    bhardlimit: u64,
    bsoftlimit: u64,
    curspace: u64,
    ihardlimit: u64,
    isoftlimit: u64,
    curinodes: u64,
    btime: u64,
    itime: u64,
    valid: u32,
    id: u32,
}
/// `struct fs_disk_quota` (include/uapi/linux/dqblk_xfs.h:105-131): 112 bytes
/// of limits, usage and expiry timers in 512-byte basic blocks.
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct XfsDiskQuota {
    d_version: i8,
    d_flags: i8,
    d_fieldmask: u16,
    d_id: u32,
    d_blk_hardlimit: u64,
    d_blk_softlimit: u64,
    d_ino_hardlimit: u64,
    d_ino_softlimit: u64,
    d_bcount: u64,
    d_icount: u64,
    d_itimer: i32,
    d_btimer: i32,
    d_iwarns: u16,
    d_bwarns: u16,
    d_itimer_hi: i8,
    d_btimer_hi: i8,
    d_rtbtimer_hi: i8,
    d_padding2: i8,
    d_rtb_hardlimit: u64,
    d_rtb_softlimit: u64,
    d_rtbcount: u64,
    d_rtbtimer: i32,
    d_rtbwarns: u16,
    d_padding3: i16,
    d_padding4: [i8; 8],
}
/// `struct fs_qfilestat` (include/uapi/linux/dqblk_xfs.h:159-163).
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct XfsQfFileStat {
    qfs_ino: u64,
    qfs_nblks: u64,
    qfs_nextents: u32,
    _pad: u32,
}
/// `struct fs_quota_stat` (include/uapi/linux/dqblk_xfs.h:165-177): the
/// Q_XGETQSTAT layout, which has room for user and group quotas only.
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct XfsQuotaStat {
    qs_version: i8,
    /// The compiler's padding byte before the 2-byte-aligned `qs_flags`.
    _pad0: u8,
    qs_flags: u16,
    qs_pad: i8,
    /// The three compiler-owned padding bytes between `qs_pad` and the
    /// 8-byte-aligned `qs_uquota`, and the four that align the whole struct to
    /// 8 bytes.  Both are spelled out so the type has no implicit padding and
    /// can be written to user memory by value.
    _padding: [u8; 3],
    qs_uquota: XfsQfFileStat,
    qs_gquota: XfsQfFileStat,
    qs_incoredqs: u32,
    qs_btimelimit: i32,
    qs_itimelimit: i32,
    qs_rtbtimelimit: i32,
    qs_bwarnlimit: u16,
    qs_iwarnlimit: u16,
    _tail: [u8; 4],
}
/// `struct fs_qfilestatv` (include/uapi/linux/dqblk_xfs.h:187-192).
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct XfsQfFileStatV {
    qfs_ino: u64,
    qfs_nblks: u64,
    qfs_nextents: u32,
    qfs_pad: u32,
}
/// `struct fs_quota_statv` (include/uapi/linux/dqblk_xfs.h:183-200): the
/// versioned layout used by Q_XGETQSTATV, with a project entry and
/// self-describing padding.
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct XfsQuotaStatV {
    qs_version: i8,
    qs_pad1: u8,
    qs_flags: u16,
    qs_incoredqs: u32,
    qs_uquota: XfsQfFileStatV,
    qs_gquota: XfsQfFileStatV,
    qs_pquota: XfsQfFileStatV,
    qs_btimelimit: i32,
    qs_itimelimit: i32,
    qs_rtbtimelimit: i32,
    qs_bwarnlimit: u16,
    qs_iwarnlimit: u16,
    qs_rtbwarnlimit: u16,
    qs_pad3: u16,
    qs_pad4: u32,
    qs_pad2: [u64; 7],
}
#[derive(Clone, Default)]
struct QuotaData {
    enabled: [bool; 3],
    records: BTreeMap<(u8, u32), IfDqblk>,
    info: [IfDqinfo; 3],
    // Keep the inode rather than the spelling supplied to Q_QUOTAON.  A quota
    // file may be reached through a bind mount or a hard link, neither of
    // which may consume the quota it controls.
    quota_files: [Option<Location>; 3],
    formats: [QuotaFormat; 3],
    dirty: bool,
}
#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum QuotaFormat {
    OldV1,
    #[default]
    V2,
}
#[derive(Default)]
struct QuotaState(Mutex<QuotaData>);

const V2_MAGICS: [u32; 3] = [0xd9c0_1f11, 0xd9c0_1927, 0xd9c0_3f14];
const V2_INFO_OFF: usize = 8;
const V2_LEAF_HEAD: usize = 16;
const V2R1_ENTRY: usize = 72;

fn put32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(buf: &mut [u8], at: usize, value: u64) {
    buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
fn get32(buf: &[u8], at: usize) -> AxResult<u32> {
    buf.get(at..at + 4)
        .and_then(|v| v.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(AxError::InvalidInput)
}
fn get64(buf: &[u8], at: usize) -> AxResult<u64> {
    buf.get(at..at + 8)
        .and_then(|v| v.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(AxError::InvalidInput)
}

fn encode_state(data: &QuotaData, ty: usize) -> AxResult<Vec<u8>> {
    let records: Vec<_> = data
        .records
        .iter()
        .filter(|((kind, _), _)| *kind as usize == ty)
        .collect();
    let mut bytes = vec![0; QUOTA_BLOCK * 2]; // header + root pointer block
    put32(&mut bytes, 0, V2_MAGICS[ty]);
    put32(&mut bytes, 4, V2_VERSION);
    put32(&mut bytes, V2_INFO_OFF, data.info[ty].bgrace as u32);
    put32(&mut bytes, V2_INFO_OFF + 4, data.info[ty].igrace as u32);
    put32(&mut bytes, V2_INFO_OFF + 8, data.info[ty].flags);
    // dqi_blocks/free_blk/free_entry are patched after every tree allocation.
    let mut nodes = BTreeMap::<(u8, u32), u32>::new();
    let allocate = |bytes: &mut Vec<u8>| -> AxResult<u32> {
        let block = (bytes.len() / QUOTA_BLOCK) as u32;
        bytes
            .try_reserve(QUOTA_BLOCK)
            .map_err(|_| AxError::NoMemory)?;
        bytes.resize(bytes.len() + QUOTA_BLOCK, 0);
        Ok(block)
    };
    for (&(_, id), record) in records {
        let mut parent = 1u32;
        for (level, shift) in [(0u8, 24u32), (1, 16), (2, 8)] {
            let prefix = id >> shift;
            let child = match nodes.get(&(level, prefix)) {
                Some(&v) => v,
                None => {
                    let v = allocate(&mut bytes)?;
                    nodes.insert((level, prefix), v);
                    v
                }
            };
            put32(
                &mut bytes,
                parent as usize * QUOTA_BLOCK + (((id >> shift) & 0xff) as usize * 4),
                child,
            );
            parent = child;
        }
        let leaf = allocate(&mut bytes)?;
        put32(
            &mut bytes,
            parent as usize * QUOTA_BLOCK + ((id & 0xff) as usize * 4),
            leaf,
        );
        let off = leaf as usize * QUOTA_BLOCK;
        bytes[off + 8..off + 10].copy_from_slice(&1u16.to_le_bytes());
        put32(&mut bytes, off + V2_LEAF_HEAD, id);
        put64(&mut bytes, off + V2_LEAF_HEAD + 8, record.ihardlimit);
        put64(&mut bytes, off + V2_LEAF_HEAD + 16, record.isoftlimit);
        put64(&mut bytes, off + V2_LEAF_HEAD + 24, record.curinodes);
        // `v2r1_mem2diskdqb()` stores the byte limits as whole QUOTABLOCK
        // (1024-byte) counts and everything else verbatim
        // (fs/quota/quota_v2.c:302-321); `v2_stoqb()` is `stoqb()`.
        put64(
            &mut bytes,
            off + V2_LEAF_HEAD + 32,
            stoqb(record.bhardlimit),
        );
        put64(
            &mut bytes,
            off + V2_LEAF_HEAD + 40,
            stoqb(record.bsoftlimit),
        );
        put64(&mut bytes, off + V2_LEAF_HEAD + 48, record.curspace);
        put64(&mut bytes, off + V2_LEAF_HEAD + 56, record.btime);
        put64(&mut bytes, off + V2_LEAF_HEAD + 64, record.itime);
    }
    let block_count = (bytes.len() / QUOTA_BLOCK) as u32;
    put32(&mut bytes, V2_INFO_OFF + 12, block_count);
    Ok(bytes)
}

// The legacy VFS quota format is an id-indexed array of eight native-endian
// u32 fields. Linux defines it as host-endian; on TheKernel's x86_64-only ABI
// that is little endian. Record zero carries the grace periods.
fn encode_v1(data: &QuotaData, ty: usize) -> AxResult<Vec<u8>> {
    let max = data
        .records
        .range((ty as u8, 0)..=(ty as u8, u32::MAX))
        .map(|((_, id), _)| *id)
        .max()
        .unwrap_or(0);
    let bytes_len = (max as usize + 1)
        .checked_mul(32)
        .ok_or(AxError::NoMemory)?;
    let mut bytes = vec![0; bytes_len];
    put32(
        &mut bytes,
        24,
        data.info[ty].igrace.min(u32::MAX as u64) as u32,
    );
    put32(
        &mut bytes,
        28,
        data.info[ty].bgrace.min(u32::MAX as u64) as u32,
    );
    for (&(_, id), r) in data.records.range((ty as u8, 0)..=(ty as u8, u32::MAX)) {
        let o = id as usize * 32;
        // `v1_mem2diskdqblk()` stores the byte limits as whole QUOTABLOCK
        // counts and the usage as a QUOTABLOCK count of `dqb_curspace`
        // (fs/quota/quota_v1.c:44-57).
        put32(&mut bytes, o, stoqb(r.bhardlimit).min(u32::MAX as u64) as u32);
        put32(
            &mut bytes,
            o + 4,
            stoqb(r.bsoftlimit).min(u32::MAX as u64) as u32,
        );
        put32(
            &mut bytes,
            o + 8,
            (r.curspace.div_ceil(1024)).min(u32::MAX as u64) as u32,
        );
        put32(&mut bytes, o + 12, r.ihardlimit.min(u32::MAX as u64) as u32);
        put32(&mut bytes, o + 16, r.isoftlimit.min(u32::MAX as u64) as u32);
        put32(&mut bytes, o + 20, r.curinodes.min(u32::MAX as u64) as u32);
        put32(&mut bytes, o + 24, r.itime.min(u32::MAX as u64) as u32);
        put32(&mut bytes, o + 28, r.btime.min(u32::MAX as u64) as u32);
    }
    Ok(bytes)
}

fn flush_locked(data: &mut QuotaData) -> AxResult<()> {
    if !data.dirty {
        return Ok(());
    }
    for (ty, file) in data.quota_files.iter().enumerate() {
        let Some(file) = file else {
            continue;
        };
        let encoded = if data.formats[ty] == QuotaFormat::OldV1 {
            encode_v1(data, ty)?
        } else {
            encode_state(data, ty)?
        };
        // Never store quota state in an implementation xattr: quota tools
        // hand us a quota file and expect that file to be authoritative.
        let written = file.entry().as_file()?.write_at(&encoded, 0)?;
        if written != encoded.len() {
            return Err(AxError::Io);
        }
        file.sync(false)?;
    }
    data.dirty = false;
    Ok(())
}

fn decode_state(bytes: &[u8], data: &mut QuotaData, ty: usize) -> AxResult<()> {
    if bytes.len() < QUOTA_BLOCK * 2
        || !bytes.len().is_multiple_of(QUOTA_BLOCK)
        || get32(bytes, 0)? != V2_MAGICS[ty]
        || get32(bytes, 4)? != V2_VERSION
    {
        return Err(AxError::InvalidInput);
    }
    let blocks = get32(bytes, V2_INFO_OFF + 12)? as usize;
    if blocks < 2 || blocks > bytes.len() / QUOTA_BLOCK {
        return Err(AxError::InvalidInput);
    }
    // Files cannot be atomically shortened by every supported backend.  The
    // validated dqi_blocks boundary is authoritative; stale tail blocks are
    // deliberately ignored rather than parsed as a second tree.
    let bytes = &bytes[..blocks * QUOTA_BLOCK];
    data.info[ty].bgrace = get32(bytes, V2_INFO_OFF)? as u64;
    data.info[ty].igrace = get32(bytes, V2_INFO_OFF + 4)? as u64;
    data.info[ty].flags = get32(bytes, V2_INFO_OFF + 8)?;
    data.records.retain(|(kind, _), _| *kind as usize != ty);
    let mut seen = BTreeSet::new();
    let mut stack = Vec::new();
    stack.push((1u32, 0u8, 0u32));
    while let Some((block, depth, prefix)) = stack.pop() {
        if block == 0 || block as usize >= blocks || !seen.insert(block) {
            return Err(AxError::InvalidInput);
        }
        let off = block as usize * QUOTA_BLOCK;
        if depth < 3 {
            for i in 0..256 {
                let next = get32(bytes, off + i * 4)?;
                if next != 0 {
                    stack.push((
                        next,
                        depth + 1,
                        prefix | ((i as u32) << (24 - depth as u32 * 8)),
                    ));
                }
            }
        } else {
            for i in 0..256 {
                let leaf = get32(bytes, off + i * 4)?;
                if leaf == 0 {
                    continue;
                }
                if leaf as usize >= blocks || !seen.insert(leaf) {
                    return Err(AxError::InvalidInput);
                }
                let lo = leaf as usize * QUOTA_BLOCK;
                let entries =
                    u16::from_le_bytes(bytes[lo + 8..lo + 10].try_into().unwrap()) as usize;
                if entries == 0 || entries > (QUOTA_BLOCK - V2_LEAF_HEAD) / V2R1_ENTRY {
                    return Err(AxError::InvalidInput);
                }
                for n in 0..entries {
                    let e = lo + V2_LEAF_HEAD + n * V2R1_ENTRY;
                    let id = get32(bytes, e)?;
                    if id != (prefix | i as u32) {
                        return Err(AxError::InvalidInput);
                    }
                    if !data
                        .records
                        .insert(
                            (ty as u8, id),
                            IfDqblk {
                                ihardlimit: get64(bytes, e + 8)?,
                                isoftlimit: get64(bytes, e + 16)?,
                                curinodes: get64(bytes, e + 24)?,
                                // `v2r1_disk2memdqb()` expands the QUOTABLOCK
                                // counts back into bytes
                                // (fs/quota/quota_v2.c:281-300).
                                bhardlimit: qbtos(get64(bytes, e + 32)?),
                                bsoftlimit: qbtos(get64(bytes, e + 40)?),
                                curspace: get64(bytes, e + 48)?,
                                btime: get64(bytes, e + 56)?,
                                itime: get64(bytes, e + 64)?,
                                ..Default::default()
                            },
                        )
                        .is_none()
                    {
                        return Err(AxError::InvalidInput);
                    }
                }
            }
        }
    }
    Ok(())
}

fn decode_v1(bytes: &[u8], data: &mut QuotaData, ty: usize) -> AxResult<()> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(32) {
        return Err(AxError::InvalidInput);
    }
    data.records.retain(|(kind, _), _| *kind as usize != ty);
    data.info[ty].igrace = get32(bytes, 24)? as u64;
    data.info[ty].bgrace = get32(bytes, 28)? as u64;
    data.info[ty].flags = 0;
    for id in 0..bytes.len() / 32 {
        let o = id * 32;
        let r = IfDqblk {
            // `v1_disk2memdqblk()`: `dqb_bhardlimit = v1_qbtos(...)`
            // (fs/quota/quota_v1.c:34-43).
            bhardlimit: qbtos(get32(bytes, o)? as u64),
            bsoftlimit: qbtos(get32(bytes, o + 4)? as u64),
            curspace: (get32(bytes, o + 8)? as u64) * 1024,
            ihardlimit: get32(bytes, o + 12)? as u64,
            isoftlimit: get32(bytes, o + 16)? as u64,
            curinodes: get32(bytes, o + 20)? as u64,
            itime: get32(bytes, o + 24)? as u64,
            btime: get32(bytes, o + 28)? as u64,
            ..Default::default()
        };
        if r.bhardlimit != 0
            || r.bsoftlimit != 0
            || r.curspace != 0
            || r.ihardlimit != 0
            || r.isoftlimit != 0
            || r.curinodes != 0
        {
            data.records.insert((ty as u8, id as u32), r);
        }
    }
    Ok(())
}

fn mark_dirty(root: &Location) {
    if let Ok(state) = state(root) {
        state.0.lock().dirty = true;
    }
}

fn quota_type(cmd: u32) -> AxResult<usize> {
    match cmd & SUBCMDMASK {
        0..=2 => Ok((cmd & SUBCMDMASK) as usize),
        _ => Err(AxError::InvalidInput),
    }
}

/// Linux passes `quotactl` commands through `QCMD(command, type)`: the low
/// byte is the quota type and the remaining bits are the command selector.
const fn quota_command(cmd: u32) -> u32 {
    cmd >> SUBCMDSHIFT
}
fn admin() -> AxResult<()> {
    current()
        .as_thread()
        .has_effective_capability(CAP_SYS_ADMIN)
        .then_some(())
        .ok_or_else(|| LinuxError::EPERM.into())
}
fn may_read(ty: usize, id: u32) -> bool {
    let task = current();
    let thread = task.as_thread();
    thread.has_effective_capability(CAP_SYS_ADMIN)
        || (ty == 0 && current().as_thread().fsuid().into_raw() == id)
        || (ty == 1
            && (thread.fsgid().into_raw() == id
                || Kgid::from_raw(id)
                    .is_some_and(|gid| thread.current_cred().groups().contains(gid))))
}
fn merge_record(old: &mut IfDqblk, new: IfDqblk) {
    let valid = new.valid;
    if valid & QIF_BLIMITS != 0 {
        old.bhardlimit = new.bhardlimit;
        old.bsoftlimit = new.bsoftlimit;
    }
    if valid & QIF_SPACE != 0 {
        old.curspace = new.curspace;
    }
    if valid & QIF_ILIMITS != 0 {
        old.ihardlimit = new.ihardlimit;
        old.isoftlimit = new.isoftlimit;
    }
    if valid & QIF_INODES != 0 {
        old.curinodes = new.curinodes;
    }
    if valid & QIF_BTIME != 0 {
        old.btime = new.btime;
    }
    if valid & QIF_ITIME != 0 {
        old.itime = new.itime;
    }
    old.valid |= valid;
}
/// `qtree_get_next_id()` (fs/quota/quota_tree.c:792-844) for a live snapshot:
/// the smallest identifier the type has a record for at or after `id`, with
/// that record.  `find_next_id()` starts at `__get_index(info, *id, depth)`,
/// which is `*id` itself, so the search is inclusive -- the uapi comment on
/// both `Q_GETNEXTQUOTA` and `Q_XGETNEXTQUOTA` reads "get disk limits and
/// usage >= ID" (include/uapi/linux/quota.h:70-74,
/// include/uapi/linux/dqblk_xfs.h:41-44).  Running out of the tree is ENOENT.
fn next_quota_record(data: &QuotaData, ty: usize, id: u32) -> AxResult<(u32, IfDqblk)> {
    data.records
        .range((ty as u8, id)..)
        .find(|((kind, _), _)| *kind == ty as u8)
        .map(|(&(_, next), record)| (next, *record))
        .ok_or_else(|| LinuxError::ENOENT.into())
}
fn merge_info(old: &mut IfDqinfo, new: IfDqinfo) {
    let valid = new.valid;
    if valid & QIF_BGRACE != 0 {
        old.bgrace = new.bgrace;
    }
    if valid & QIF_IGRACE != 0 {
        old.igrace = new.igrace;
    }
    if valid & QIF_FLAGS != 0 {
        old.flags = new.flags;
    }
    old.valid |= valid;
}
/// `lookup_bdev()` followed by `user_get_super()`: the `special` argument of
/// `quotactl` names a block device, and the filesystem the command operates on
/// is whichever live superblock is backed by that device.
///
///   error = kern_path(pathname, LOOKUP_FOLLOW, &path);      /* ENOENT, ... */
///   if (!S_ISBLK(inode->i_mode)) { error = -ENOTBLK; ... }
///   if (!may_open_dev(&path))    { error = -EACCES;  ... }
///   sb = user_get_super(dev, excl); if (!sb) return ERR_PTR(-ENODEV);
fn root_for_device<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const c_char,
) -> AxResult<Location> {
    let bytes = vm_load_until_nul(memory, ptr.cast()).map_err(map_usercopy_error)?;
    // lookup_bdev(): `if (!pathname || !*pathname) return -EINVAL;`
    if bytes.is_empty() {
        return Err(AxError::InvalidInput);
    }
    let path = FsPathBuf::from_vec(bytes);
    validate_pathname(&path)?;
    let security = VfsSecurityContext::new(current().as_thread().current_cred());
    let loc = match resolve_at_with_security(AT_FDCWD, Some(&path), 0, &security)? {
        ResolveAtResult::File(loc) => loc,
        ResolveAtResult::Other(_) => return Err(AxError::InvalidInput),
    };
    let metadata = loc.metadata()?;
    let is_block_device = metadata.node_type == NodeType::BlockDevice;
    let nodev = crate::mounts::is_nodev(&loc)?;
    let superblock = if is_block_device && !nodev {
        crate::mounts::mounted_root_location(metadata.rdev).ok()
    } else {
        None
    };
    match linux_vfs::admit_quota_device(is_block_device, nodev, superblock.is_some()) {
        Ok(()) => Ok(superblock.expect("admitted device has a superblock")),
        Err(linux_vfs::QuotaDeviceReject::NotBlockDevice) => Err(LinuxError::ENOTBLK.into()),
        Err(linux_vfs::QuotaDeviceReject::Nodev) => Err(AxError::PermissionDenied),
        Err(linux_vfs::QuotaDeviceReject::NoSuperblock) => Err(LinuxError::ENODEV.into()),
    }
}

/// Resolves the `Q_QUOTAON` quota-file path.  Linux resolves it in the syscall
/// before the superblock lookup and reports a failure only from
/// `quota_quotaon()`, after the provider and permission decisions.
fn quota_file_for_on<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    addr: usize,
) -> AxResult<Location> {
    let path = vm_load_until_nul(memory, addr as *const u8).map_err(map_usercopy_error)?;
    if path.is_empty() {
        return Err(LinuxError::ENOENT.into());
    }
    let path = FsPathBuf::from_vec(path);
    validate_pathname(&path)?;
    let security = VfsSecurityContext::new(current().as_thread().current_cred());
    match resolve_at_with_security(AT_FDCWD, Some(&path), 0, &security)? {
        ResolveAtResult::File(file) => Ok(file),
        ResolveAtResult::Other(_) => Err(AxError::InvalidInput),
    }
}

fn location_for_fd(fd: i32) -> AxResult<Location> {
    let file = get_file_like(fd)?;
    let loc = if let Some(file) = file.downcast_ref::<File>() {
        file.inner().location().clone()
    } else if let Some(dir) = file.downcast_ref::<Directory>() {
        dir.inner().clone()
    } else {
        // A descriptor with no path - a pipe, socket or anonymous inode - has
        // no mount to name.  Linux reaches do_quotactl() through the
        // descriptor's mount and reports the missing provider as ENOSYS.
        return Err(AxError::Unsupported);
    };
    Ok(loc)
}
fn state(root: &Location) -> AxResult<Arc<QuotaState>> {
    root.user_data().try_get_or_insert_with(QuotaState::default)
}

fn root_for_location(loc: &Location) -> Location {
    loc.mountpoint().root_location()
}

fn owners(metadata: &axfs_ng_vfs::Metadata) -> [u32; 3] {
    [metadata.uid, metadata.gid, metadata.project_id]
}

/// Mount-wide inode enumeration used by Q_QUOTAON.
///
/// A directory walk is not an accounting oracle: it misses an unlinked inode
/// still held open and it describes hard links more than once. Ask the mounted
/// filesystem for its live-inode registry instead. Backends which cannot make
/// that complete promise fail with EOPNOTSUPP through the VFS contract.
fn seed_usage(root: &Location, data: &mut QuotaData, ty: usize) -> AxResult<()> {
    for ((kind, _), record) in data.records.iter_mut() {
        if *kind as usize == ty {
            record.curinodes = 0;
            record.curspace = 0;
        }
    }
    let quota_inodes: Vec<_> = data
        .quota_files
        .iter()
        .flatten()
        .filter(|&file| file.same_mount(root))
        .map(|file| file.inode())
        .collect();
    root.filesystem().enumerate_inodes(&mut |metadata| {
        // unlink refunds quota at namespace removal, not at final close.
        // Backends may keep a zero-link inode physically allocated while an
        // open descriptor pins it; it must not be recharged on activation.
        if metadata.nlink == 0 {
            return Ok(());
        }
        if quota_inodes.contains(&metadata.inode) {
            return Ok(());
        }
        let owner = owners(&metadata)[ty];
        let record = data.records.entry((ty as u8, owner)).or_default();
        record.curinodes = record.curinodes.checked_add(1).ok_or(LinuxError::ENOSPC)?;
        record.curspace = record
            .curspace
            .checked_add(metadata.blocks.saturating_mul(512))
            .ok_or(LinuxError::ENOSPC)?;
        Ok(())
    })?;
    Ok(())
}

fn limit_reached(record: &mut IfDqblk, info: IfDqinfo, space: i128, inodes: i128) -> AxResult<()> {
    let now = crate::time::wall_time().as_secs();
    let next_space = (record.curspace as i128)
        .checked_add(space)
        .ok_or(LinuxError::ENOSPC)?;
    let next_inodes = (record.curinodes as i128)
        .checked_add(inodes)
        .ok_or(LinuxError::ENOSPC)?;
    if next_space < 0 || next_inodes < 0 {
        return Err(AxError::BadState);
    }
    let check = |next: u64, hard: u64, soft: u64, time: &mut u64, grace: u64| -> AxResult<()> {
        // `mem_dqblk` counts bytes (`qsize_t`), so a limit and the usage it
        // bounds are directly comparable.
        if hard != 0 && next > hard {
            return Err(LinuxError::EDQUOT.into());
        }
        if soft != 0 && next > soft {
            if *time == 0 {
                *time = now.saturating_add(grace);
            } else if now >= *time {
                return Err(LinuxError::EDQUOT.into());
            }
        } else {
            *time = 0;
        }
        Ok(())
    };
    check(
        next_space as u64,
        record.bhardlimit,
        record.bsoftlimit,
        &mut record.btime,
        info.bgrace,
    )?;
    // inode limits are counts, not KiB blocks.
    if record.ihardlimit != 0 && next_inodes as u64 > record.ihardlimit {
        return Err(LinuxError::EDQUOT.into());
    }
    if record.isoftlimit != 0 && next_inodes as u64 > record.isoftlimit {
        if record.itime == 0 {
            record.itime = now.saturating_add(info.igrace);
        } else if now >= record.itime {
            return Err(LinuxError::EDQUOT.into());
        }
    } else {
        record.itime = 0;
    }
    record.curspace = next_space as u64;
    record.curinodes = next_inodes as u64;
    Ok(())
}

/// A charged VFS mutation.  Charges are made before the backend mutation and
/// are undone unless the caller commits after that mutation succeeds.
pub(crate) struct QuotaCharge {
    root: Location,
    owners: [u32; 3],
    enabled: [bool; 3],
    space: i128,
    baseline_space: Option<i128>,
    inodes: i128,
    committed: bool,
}
impl QuotaCharge {
    pub(crate) fn commit(mut self) {
        mark_dirty(&self.root);
        self.committed = true;
    }
    /// Settles a conservative write reservation to the filesystem's actual
    /// 512-byte allocation count after publication.
    pub(crate) fn commit_actual_blocks(mut self, location: &Location) -> AxResult<()> {
        if let Some(before) = self.baseline_space {
            let actual = location.metadata()?.blocks as i128 * 512 - before;
            let adjustment = actual - self.space;
            if adjustment != 0 {
                let state = state(&self.root)?;
                let mut data = state.0.lock();
                for (kind, id) in self.owners.iter().copied().enumerate() {
                    if !self.enabled[kind] {
                        continue;
                    }
                    let info = data.info[kind];
                    limit_reached(
                        data.records.entry((kind as u8, id)).or_default(),
                        info,
                        adjustment,
                        0,
                    )?;
                }
            }
        }
        self.committed = true;
        mark_dirty(&self.root);
        Ok(())
    }
}
impl Drop for QuotaCharge {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(state) = state(&self.root) {
            let mut data = state.0.lock();
            for (kind, id) in self.owners.iter().copied().enumerate() {
                if !self.enabled[kind] {
                    continue;
                }
                let info = data.info[kind];
                let record = data.records.entry((kind as u8, id)).or_default();
                // Reversal cannot cross a limit; an internal inconsistency is
                // deliberately contained rather than escaping Drop.
                let _ = limit_reached(record, info, -self.space, -self.inodes);
            }
        }
    }
}

fn charge(
    loc: &Location,
    metadata: &axfs_ng_vfs::Metadata,
    space: i128,
    inodes: i128,
) -> AxResult<QuotaCharge> {
    let root = root_for_location(loc);
    let state = state(&root)?;
    let owners = owners(metadata);
    let mut data = state.0.lock();
    if data
        .quota_files
        .iter()
        .flatten()
        .any(|file| file.same_node(loc))
    {
        return Ok(QuotaCharge {
            root,
            owners,
            enabled: [false; 3],
            space: 0,
            baseline_space: None,
            inodes: 0,
            committed: true,
        });
    }
    // All enabled dimensions are admitted as one transaction.  Roll back the
    // dimensions already charged when a later one rejects the operation.
    let mut charged = 0;
    for (kind, id) in owners.iter().copied().enumerate() {
        if !data.enabled[kind] {
            continue;
        }
        let info = data.info[kind];
        let record = data.records.entry((kind as u8, id)).or_default();
        // Grace deadlines are part of the transaction too: never leave a
        // failed later dimension with a freshly armed deadline.
        let mut updated = *record;
        if let Err(error) = limit_reached(&mut updated, info, space, inodes) {
            for (rollback_kind, rollback_owner) in owners.iter().copied().take(charged).enumerate()
            {
                if !data.enabled[rollback_kind] {
                    continue;
                }
                let rollback_info = data.info[rollback_kind];
                let rollback = data
                    .records
                    .entry((rollback_kind as u8, rollback_owner))
                    .or_default();
                let _ = limit_reached(rollback, rollback_info, -space, -inodes);
            }
            return Err(error);
        }
        *record = updated;
        charged = kind + 1;
    }
    Ok(QuotaCharge {
        root,
        owners,
        enabled: data.enabled,
        space,
        baseline_space: None,
        inodes,
        committed: false,
    })
}

pub(crate) fn admit_inode_create(
    parent: &Location,
    metadata: &axfs_ng_vfs::Metadata,
) -> AxResult<QuotaCharge> {
    charge(parent, metadata, 0, 1)
}
pub(crate) fn admit_resize(loc: &Location, _old_len: u64, new_len: u64) -> AxResult<QuotaCharge> {
    let metadata = loc.metadata()?;
    let before = metadata.blocks as i128 * 512;
    // This is a bound, not an assertion about allocation: sparse backends may
    // allocate less and are settled from Metadata.blocks after success.
    let predicted = (new_len.div_ceil(512) as i128 * 512).max(before);
    let mut charge = charge(loc, &metadata, predicted - before, 0)?;
    charge.baseline_space = Some(before);
    Ok(charge)
}
pub(crate) fn admit_unlink(
    loc: &Location,
    metadata: &axfs_ng_vfs::Metadata,
) -> AxResult<QuotaCharge> {
    charge(loc, metadata, -(metadata.blocks as i128 * 512), -1)
}
pub(crate) fn admit_chown(
    loc: &Location,
    old: &axfs_ng_vfs::Metadata,
    new: &axfs_ng_vfs::Metadata,
) -> AxResult<(QuotaCharge, QuotaCharge)> {
    // Transfer is reserved against the new owner first; both guards make the
    // operation rollback-safe if metadata publication fails.
    let space = old.blocks as i128 * 512;
    Ok((charge(loc, new, space, 1)?, charge(loc, old, -space, -1)?))
}
fn read_struct<M: UserMemory + ?Sized, T: bytemuck::Pod>(
    memory: &mut UserMemoryContext<'_, M>,
    addr: usize,
) -> AxResult<T> {
    // `copy_from_user()` on a NULL pointer is EFAULT, and every caller reaches
    // this helper only after the command's own validation.
    if addr == 0 {
        return Err(LinuxError::EFAULT.into());
    }
    let bytes = vm_load(memory, addr as *const u8, core::mem::size_of::<T>())
        .map_err(map_usercopy_error)?;
    Ok(bytemuck::pod_read_unaligned(&bytes))
}
fn write_struct<M: UserMemory + ?Sized, T: bytemuck::NoUninit>(
    memory: &mut UserMemoryContext<'_, M>,
    addr: usize,
    value: &T,
) -> AxResult<()> {
    // See `read_struct()`: a NULL destination is EFAULT before any copy.
    if addr == 0 {
        return Err(LinuxError::EFAULT.into());
    }
    vm_write_slice(memory, addr as *mut u8, bytemuck::bytes_of(value)).map_err(map_usercopy_error)
}
/// `quota_btobb()` (fs/quota/quota.c:534-537): bytes to 512-byte basic
/// blocks, rounding up.
const fn xfs_blocks_from_bytes(bytes: u64) -> u64 {
    (bytes + (1 << XFS_BB_SHIFT) - 1) >> XFS_BB_SHIFT
}
/// `quota_bbtob()` (fs/quota/quota.c:529-532): basic blocks back to bytes.
const fn xfs_bytes_from_blocks(blocks: u64) -> u64 {
    blocks << XFS_BB_SHIFT
}
/// `stoqb()` (fs/quota/quota.c:182-185): bytes as whole 1024-byte quota
/// blocks, rounding up.  Linux's `space + QIF_DQBLKSIZE - 1` can only wrap for
/// a limit which `do_set_dqblk()`'s ERANGE check already rejected, so the
/// round-up form is exact everywhere it is reachable.
const fn stoqb(bytes: u64) -> u64 {
    (bytes + QIF_DQBLKSIZE - 1) >> QIF_DQBLKSIZE_BITS
}
/// `qbtos()` (fs/quota/quota.c:177-180): whole quota blocks back to bytes.
/// The shift truncates exactly like Linux's, including for a request whose
/// high bits fall off the end.
const fn qbtos(blocks: u64) -> u64 {
    blocks << QIF_DQBLKSIZE_BITS
}
/// `dqi_max_spc_limit` and `dqi_max_ino_limit`, as each format's
/// `read_file_info()` installs them.  The legacy format stores unsigned
/// 32-bit quota-block counts, so its space ceiling is `0xffffffff` blocks, not
/// `0xffffffff` bytes (fs/quota/quota_v1.c:177-178, and the same pair for the
/// version-0 v2 format in fs/quota/quota_v2.c:134-145); the current format
/// stores 64-bit byte counts and ceilings both at `2^63-1`
/// (fs/quota/quota_v2.c:141-142).
const fn max_limits(format: QuotaFormat) -> (u64, u64) {
    match format {
        QuotaFormat::OldV1 => (0xffff_ffffu64 << QIF_DQBLKSIZE_BITS, 0xffff_ffff),
        QuotaFormat::V2 => (0x7fff_ffff_ffff_ffff, 0x7fff_ffff_ffff_ffff),
    }
}
/// `do_set_dqblk()`'s `if (di->d_fieldmask & ~VFS_QC_MASK) return -EINVAL;`
/// (fs/quota/dquot.c:2740-2754) seen through `copy_from_xfs_dqblk()`
/// (fs/quota/quota.c:546-597), which translates each XFS selector into one
/// `QC_*` bit.  `VFS_QC_MASK` is exactly the block- and inode-limit, accounting
/// and grace selectors, so the realtime groups, every warning count and the
/// realtime block count are rejected instead of silently accepted.
/// `FS_DQ_BIGTIME` is a timer-width flag rather than a selector and maps to no
/// `QC_*` bit, which is why it is allowed through.
fn qc_mask_from_xfs(fieldmask: u16) -> u32 {
    let fieldmask = u32::from(fieldmask);
    let mut qc = 0;
    for (xfs, bit) in XFS_QC_SELECTORS {
        if fieldmask & xfs != 0 {
            qc |= bit;
        }
    }
    qc
}
/// `copy_from_if_dqblk()`'s fieldmask translation
/// (fs/quota/quota.c:213-224).  A set bit which `struct if_dqblk` does not
/// define selects nothing, exactly like Linux.
fn qc_mask_from_if(valid: u32) -> u32 {
    let mut qc = 0;
    if valid & QIF_BLIMITS != 0 {
        qc |= QC_SPC_SOFT | QC_SPC_HARD;
    }
    if valid & QIF_SPACE != 0 {
        qc |= QC_SPACE;
    }
    if valid & QIF_ILIMITS != 0 {
        qc |= QC_INO_SOFT | QC_INO_HARD;
    }
    if valid & QIF_INODES != 0 {
        qc |= QC_INO_COUNT;
    }
    if valid & QIF_BTIME != 0 {
        qc |= QC_SPC_TIMER;
    }
    if valid & QIF_ITIME != 0 {
        qc |= QC_INO_TIMER;
    }
    qc
}
/// `do_set_dqblk()`'s validation half (fs/quota/dquot.c:2753-2762): a selector
/// with no meaning in the VFS domain is EINVAL, and a limit the request
/// actually selected above the format's maximum is ERANGE rather than a silent
/// truncation.  Both decisions are taken before anything is stored.
fn check_set_dqblk(qc: u32, record: &IfDqblk, format: QuotaFormat) -> AxResult<()> {
    if qc & !VFS_QC_MASK != 0 {
        return Err(AxError::InvalidInput);
    }
    let (max_spc, max_ino) = max_limits(format);
    if qc & QC_SPC_SOFT != 0 && record.bsoftlimit > max_spc {
        return Err(LinuxError::ERANGE.into());
    }
    if qc & QC_SPC_HARD != 0 && record.bhardlimit > max_spc {
        return Err(LinuxError::ERANGE.into());
    }
    if qc & QC_INO_SOFT != 0 && record.isoftlimit > max_ino {
        return Err(LinuxError::ERANGE.into());
    }
    if qc & QC_INO_HARD != 0 && record.ihardlimit > max_ino {
        return Err(LinuxError::ERANGE.into());
    }
    Ok(())
}
/// `copy_from_xfs_dqblk_ts()` (fs/quota/quota.c:539-545): a 40-bit quota timer
/// is `(u32)timer | (s64)timer_hi << 32` only when `FS_DQ_BIGTIME` is set;
/// otherwise the 32-bit low half is sign extended.
const fn xfs_timer_from_wire(low: i32, high: i8, fieldmask: u16) -> u64 {
    if fieldmask & FS_DQ_BIGTIME as u16 != 0 {
        (low as u32 as u64) | ((high as i64 as u64) << 32)
    } else {
        low as i64 as u64
    }
}
/// `copy_from_xfs_dqblk()` field-mask translation
/// (fs/quota/quota.c:566-591).  Realtime and warning groups map to `QC_*`
/// bits that `do_set_dqblk()` accepts but does not act on for a dquot
/// provider, and `FS_DQ_BIGTIME` is not a field selector, so neither sets a
/// VFS `dqb_valid` bit.
const fn qif_valid_from_xfs(fieldmask: u32) -> u32 {
    let mut valid = 0;
    if fieldmask & (FS_DQ_BSOFT | FS_DQ_BHARD) != 0 {
        valid |= QIF_BLIMITS;
    }
    if fieldmask & FS_DQ_BCOUNT != 0 {
        valid |= QIF_SPACE;
    }
    if fieldmask & (FS_DQ_ISOFT | FS_DQ_IHARD) != 0 {
        valid |= QIF_ILIMITS;
    }
    if fieldmask & FS_DQ_ICOUNT != 0 {
        valid |= QIF_INODES;
    }
    if fieldmask & FS_DQ_BTIMER != 0 {
        valid |= QIF_BTIME;
    }
    if fieldmask & FS_DQ_ITIMER != 0 {
        valid |= QIF_ITIME;
    }
    valid
}
/// `copy_from_xfs_dqblk()` as a whole (fs/quota/quota.c:546-597).
fn if_dqblk_from_xfs(src: &XfsDiskQuota) -> IfDqblk {
    IfDqblk {
        bhardlimit: xfs_bytes_from_blocks(src.d_blk_hardlimit),
        bsoftlimit: xfs_bytes_from_blocks(src.d_blk_softlimit),
        curspace: xfs_bytes_from_blocks(src.d_bcount),
        ihardlimit: src.d_ino_hardlimit,
        isoftlimit: src.d_ino_softlimit,
        curinodes: src.d_icount,
        btime: xfs_timer_from_wire(src.d_btimer, src.d_btimer_hi, src.d_fieldmask),
        itime: xfs_timer_from_wire(src.d_itimer, src.d_itimer_hi, src.d_fieldmask),
        valid: qif_valid_from_xfs(src.d_fieldmask as u32),
        _pad: 0,
    }
}
/// `copy_to_if_dqblk()` (fs/quota/quota.c:186-198) publishes the byte-unit
/// block limits as whole 1024-byte quota blocks (`stoqb()`); every other field
/// is already in the unit `struct if_dqblk` declares.
fn if_dqblk_wire(record: &IfDqblk) -> IfDqblk {
    IfDqblk {
        bhardlimit: stoqb(record.bhardlimit),
        bsoftlimit: stoqb(record.bsoftlimit),
        ..*record
    }
}
/// `copy_from_if_dqblk()` (fs/quota/quota.c:200-224) for a `Q_SETQUOTA`
/// request.  `d_fieldmask` is rebuilt from `dqb_valid` only, so a set bit with
/// no meaning in `struct if_dqblk` is dropped rather than rejected, and the
/// block limits arrive in 1024-byte units.
fn if_dqblk_from_wire(wire: IfDqblk) -> IfDqblk {
    IfDqblk {
        bhardlimit: qbtos(wire.bhardlimit),
        bsoftlimit: qbtos(wire.bsoftlimit),
        ..wire
    }
}
/// `quota_getnextquota()`'s output (fs/quota/quota.c:377-399): "struct
/// if_nextdqblk is a superset of struct if_dqblk", so the same conversion
/// applies and only the identifier is added.
fn if_next_dqblk_wire(record: &IfDqblk, id: u32) -> IfNextDqblk {
    let wire = if_dqblk_wire(record);
    IfNextDqblk {
        bhardlimit: wire.bhardlimit,
        bsoftlimit: wire.bsoftlimit,
        curspace: wire.curspace,
        ihardlimit: wire.ihardlimit,
        isoftlimit: wire.isoftlimit,
        curinodes: wire.curinodes,
        btime: wire.btime,
        itime: wire.itime,
        valid: DQBLK_VALID_MASK,
        id,
    }
}
/// `copy_to_xfs_dqblk()` (fs/quota/quota.c:672-701).
fn xfs_disk_quota(data: &QuotaData, ty: usize, id: u32) -> XfsDiskQuota {
    let record = data
        .records
        .get(&(ty as u8, id))
        .copied()
        .unwrap_or_default();
    let bigtime = record.btime > i32::MAX as u64
        || record.btime < i32::MIN as i64 as u64
        || record.itime > i32::MAX as u64
        || record.itime < i32::MIN as i64 as u64;
    let (btimer_hi, itimer_hi) = if bigtime {
        ((record.btime >> 32) as i8, (record.itime >> 32) as i8)
    } else {
        (0, 0)
    };
    XfsDiskQuota {
        d_version: FS_DQUOT_VERSION,
        d_flags: match ty {
            0 => FS_USER_QUOTA,
            2 => FS_PROJ_QUOTA,
            _ => FS_GROUP_QUOTA,
        },
        d_fieldmask: if bigtime { FS_DQ_BIGTIME as u16 } else { 0 },
        d_id: id,
        d_blk_hardlimit: xfs_blocks_from_bytes(record.bhardlimit),
        d_blk_softlimit: xfs_blocks_from_bytes(record.bsoftlimit),
        d_ino_hardlimit: record.ihardlimit,
        d_ino_softlimit: record.isoftlimit,
        d_bcount: xfs_blocks_from_bytes(record.curspace),
        d_icount: record.curinodes,
        d_itimer: record.itime as u32 as i32,
        d_btimer: record.btime as u32 as i32,
        d_itimer_hi: itimer_hi,
        d_btimer_hi: btimer_hi,
        ..Default::default()
    }
}
/// `quota_state_to_flags()` (include/linux/quota.h): one accounting bit and one
/// enforcement bit per active quota type.  Every TheKernel activation is a
/// `dquot_load_quota_inode(..., DQUOT_USAGE_ENABLED | DQUOT_LIMITS_ENABLED)`,
/// so an enabled type reports both.
fn xfs_state_flags(data: &QuotaData) -> u16 {
    let mut flags = 0;
    for (ty, enabled) in data.enabled.iter().enumerate() {
        if !enabled {
            continue;
        }
        flags |= 1u16 << (2 * ty);
        flags |= 1u16 << (2 * ty + 1);
    }
    flags
}
/// `dquot_get_state()` fills the quota-file accounting from the live quota
/// inode: `tstate->ino = dqopt->files[type]->i_ino`,
/// `tstate->blocks = dqopt->files[type]->i_blocks` and
/// `tstate->nextents = 1; /* We don't know... */` (fs/quota/dquot.c:2885).
/// It leaves `s_incoredqs`, every warning limit and every realtime field at
/// zero, which is what `quota_getstate()` then reports.
fn xfs_file_stat(data: &QuotaData, ty: usize) -> Option<[u64; 3]> {
    let metadata = data.quota_files[ty].as_ref()?.metadata().ok()?;
    Some([metadata.inode, metadata.blocks, 1])
}
fn xfs_quota_stat(data: &QuotaData, ty: usize) -> XfsQuotaStat {
    let file = |ty: usize| -> XfsQfFileStat {
        match xfs_file_stat(data, ty) {
            Some([ino, nblks, nextents]) => XfsQfFileStat {
                qfs_ino: ino,
                qfs_nblks: nblks,
                qfs_nextents: nextents as u32,
                _pad: 0,
            },
            None => XfsQfFileStat::default(),
        }
    };
    // `quota_getstate()` (fs/quota/quota.c:377-425): project-quota storage is
    // reported in the group slot only while group accounting is disabled,
    // because `fs_quota_stat` has no third slot.
    let gquota = if data.enabled[1] {
        file(1)
    } else {
        file(2)
    };
    XfsQuotaStat {
        qs_version: FS_QSTAT_VERSION,
        _pad0: 0,
        qs_flags: xfs_state_flags(data),
        qs_pad: 0,
        _padding: [0; 3],
        _tail: [0; 4],
        qs_uquota: file(0),
        qs_gquota: gquota,
        qs_incoredqs: 0,
        qs_btimelimit: data.info[ty].bgrace as u32 as i32,
        qs_itimelimit: data.info[ty].igrace as u32 as i32,
        qs_rtbtimelimit: 0,
        qs_bwarnlimit: 0,
        qs_iwarnlimit: 0,
    }
}
fn xfs_quota_statv(data: &QuotaData, ty: usize) -> XfsQuotaStatV {
    let file = |ty: usize| -> XfsQfFileStatV {
        match xfs_file_stat(data, ty) {
            Some([ino, nblks, nextents]) => XfsQfFileStatV {
                qfs_ino: ino,
                qfs_nblks: nblks,
                qfs_nextents: nextents as u32,
                qfs_pad: 0,
            },
            None => XfsQfFileStatV::default(),
        }
    };
    XfsQuotaStatV {
        qs_version: FS_QSTAT_VERSION,
        qs_pad1: 0,
        qs_flags: xfs_state_flags(data),
        qs_incoredqs: 0,
        qs_uquota: file(0),
        qs_gquota: file(1),
        qs_pquota: file(2),
        qs_btimelimit: data.info[ty].bgrace as u32 as i32,
        qs_itimelimit: data.info[ty].igrace as u32 as i32,
        qs_rtbtimelimit: 0,
        qs_bwarnlimit: 0,
        qs_iwarnlimit: 0,
        qs_rtbwarnlimit: 0,
        qs_pad3: 0,
        qs_pad4: 0,
        qs_pad2: [0; 7],
    }
}
/// The Q_XGETQSTAT form of `quota_getstate()`'s "No quota enabled?" test
/// (fs/quota/quota.c:392-394): one active type anywhere on the superblock is
/// enough, and the requested type only selects the timer limits.
fn any_quota_active(data: &QuotaData) -> bool {
    data.enabled.iter().any(|enabled| *enabled)
}
/// `do_quotactl()`: provider support, quota-type support, the permission table
/// and then the command itself.
///
///   if (!sb->s_qcop) return -ENOSYS;
///   if (!(sb->s_quota_types & (1 << type))) return -EINVAL;
///   ret = check_quotactl_permission(sb, type, cmd, id);
///   if (ret < 0) return ret;
///   switch (cmd) { ... }
fn quotactl<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    root: Location,
    cmd: u32,
    id: u32,
    addr: usize,
    on_path: AxResult<Location>,
    read_only: bool,
) -> AxResult<isize> {
    let op = quota_command(cmd);
    let ty = quota_type(cmd)?;
    if read_only {
        // `quotactl_fd()` runs `mnt_want_write()` for the commands of
        // `quotactl_cmd_write()` before it ever reads the superblock.
        return Err(AxError::ReadOnlyFilesystem);
    }
    // FAT has no durable project-id representation.  Linux reports a missing
    // quotactl provider here, before it checks permissions.
    if ty == 2 && root.filesystem().name() == "fat" {
        return Err(AxError::OperationNotSupported);
    }
    match linux_vfs::quota_command_privilege(op) {
        linux_vfs::QuotaCommandPrivilege::None => {}
        // Q_GETQUOTA and Q_XGETQUOTA admit the caller's own identifier.
        linux_vfs::QuotaCommandPrivilege::OwnIdentity if may_read(ty, id) => {}
        _ => admin()?,
    }
    let state = state(&root)?;
    match op {
        Q_SYNC => {
            // dquot_quota_sync() writes back whatever is active and succeeds
            // when a type is not enabled.
            let mut data = state.0.lock();
            flush_locked(&mut data)?;
            Ok(0)
        }
        Q_QUOTAON => {
            // quota_quotaon() reports the deferred quota-file lookup error.
            let quota_file = on_path?;
            // dquot_quota_on(): the quota file must live on the same
            // superblock.
            if !quota_file.same_mount(&root) {
                return Err(LinuxError::EXDEV.into());
            }
            let metadata = quota_file.metadata()?;
            // vfs_setup_quota_inode(): a regular, writable quota file that is
            // not already carrying this quota type.
            if metadata.node_type != NodeType::RegularFile {
                return Err(AxError::PermissionDenied);
            }
            if crate::mounts::is_readonly(&quota_file)? {
                return Err(AxError::ReadOnlyFilesystem);
            }
            let mut q = state.0.lock();
            if q.enabled[ty] {
                return Err(AxError::ResourceBusy);
            }
            // dquot_load_quota_sb(): find_quota_format() -> ESRCH.
            if id != QFMT_VFS_V1 && id != QFMT_VFS_OLD {
                return Err(LinuxError::ESRCH.into());
            }
            // Everything below is fallible, including parsing the supplied
            // file and enumerating the mount. Keep the published state intact
            // until both have succeeded.
            let mut next = q.clone();
            let len = usize::try_from(metadata.size).map_err(|_| AxError::InvalidInput)?;
            if len > MAX_QUOTA_FILE_BYTES {
                return Err(AxError::InvalidInput);
            }
            if len != 0 {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(len)
                    .map_err(|_| AxError::NoMemory)?;
                bytes.resize(len, 0);
                let read = quota_file.entry().as_file()?.read_at(&mut bytes, 0)?;
                bytes.truncate(read);
                next.formats[ty] = if bytes.len() >= 8 && get32(&bytes, 0).ok() == Some(V2_MAGICS[ty])
                {
                    QuotaFormat::V2
                } else {
                    QuotaFormat::OldV1
                };
                if next.formats[ty] == QuotaFormat::V2 {
                    decode_state(&bytes, &mut next, ty)?;
                } else {
                    decode_v1(&bytes, &mut next, ty)?;
                }
                // Each quota type is explicitly activated by Q_QUOTAON;
                // persisted enabled bits describe the prior clean state,
                // not an implicit mount-time activation.
                next.enabled = [false; 3];
            }
            next.formats[ty] = if id == QFMT_VFS_OLD {
                QuotaFormat::OldV1
            } else {
                QuotaFormat::V2
            };
            next.quota_files[ty] = Some(quota_file);
            // Existing inodes predate quota activation.  Their current
            // uid/gid/project and allocated 512-byte block count seed the
            // ledger before new mutations are admitted.
            seed_usage(&root, &mut next, ty)?;
            next.enabled[ty] = true;
            next.dirty = true;
            *q = next;
            Ok(0)
        }
        Q_QUOTAOFF => {
            let mut q = state.0.lock();
            // dquot_disable() returns 0 when nothing is loaded; disabling an
            // inactive type is a successful no-op.
            if !q.enabled[ty] {
                return Ok(0);
            }
            flush_locked(&mut q)?;
            q.enabled[ty] = false;
            q.quota_files[ty] = None;
            Ok(0)
        }
        Q_GETFMT => {
            let q = state.0.lock();
            // quota_getfmt(): `if (!sb_has_quota_active(sb, type)) return -ESRCH;`
            if !q.enabled[ty] {
                return Err(LinuxError::ESRCH.into());
            }
            let format = match q.formats[ty] {
                QuotaFormat::OldV1 => QFMT_VFS_OLD,
                QuotaFormat::V2 => QFMT_VFS_V1,
            };
            write_struct(memory, addr, &format)?;
            Ok(0)
        }
        Q_GETINFO => {
            let q = state.0.lock();
            // quota_getinfo(): the acct-enabled flag of get_state() -> ESRCH.
            if !q.enabled[ty] {
                return Err(LinuxError::ESRCH.into());
            }
            let info = IfDqinfo {
                valid: DQINFO_VALID_MASK,
                ..q.info[ty]
            };
            write_struct(memory, addr, &info)?;
            Ok(0)
        }
        Q_SETINFO => {
            // quota_setinfo() copies the structure before it validates it.
            let new: IfDqinfo = read_struct(memory, addr)?;
            if new.valid & !DQINFO_VALID_MASK != 0 {
                return Err(AxError::InvalidInput);
            }
            if new.valid & QIF_FLAGS != 0 && new.flags & !DQF_SETINFO_MASK != 0 {
                return Err(AxError::InvalidInput);
            }
            let mut q = state.0.lock();
            // dquot_set_dqinfo(): `if (!sb_has_quota_active(sb, type)) return -ESRCH;`
            if !q.enabled[ty] {
                return Err(LinuxError::ESRCH.into());
            }
            if new.valid & QIF_FLAGS != 0
                && new.flags & DQF_ROOT_SQUASH != 0
                && q.formats[ty] != QuotaFormat::OldV1
            {
                // Root squash is only representable in the old format.
                return Err(AxError::InvalidInput);
            }
            merge_info(&mut q.info[ty], new);
            q.dirty = true;
            Ok(0)
        }
        Q_GETQUOTA => {
            let q = state.0.lock();
            // dquot_get_dqblk() -> dqget() -> ESRCH when the type is inactive.
            if !q.enabled[ty] {
                return Err(LinuxError::ESRCH.into());
            }
            let record = q.records.get(&(ty as u8, id)).copied().unwrap_or_default();
            write_struct(
                memory,
                addr,
                &IfDqblk {
                    valid: DQBLK_VALID_MASK,
                    ..if_dqblk_wire(&record)
                },
            )?;
            Ok(0)
        }
        Q_SETQUOTA => {
            // quota_setquota() copies the structure first; unknown `dqb_valid`
            // bits are ignored rather than rejected.
            let wire: IfDqblk = read_struct(memory, addr)?;
            let record = if_dqblk_from_wire(wire);
            let mut q = state.0.lock();
            // dquot_set_dqblk() -> dqget() -> ESRCH when the type is inactive.
            if !q.enabled[ty] {
                return Err(LinuxError::ESRCH.into());
            }
            // do_set_dqblk() checks the field mask, then the format's maximum,
            // and only then applies anything (fs/quota/dquot.c:2753-2762).
            // `Q_SETQUOTA` can only select the six `QIF_*` groups
            // `copy_from_if_dqblk()` maps, which never trip the mask check.
            check_set_dqblk(qc_mask_from_if(record.valid), &record, q.formats[ty])?;
            merge_record(q.records.entry((ty as u8, id)).or_default(), record);
            q.dirty = true;
            Ok(0)
        }
        Q_GETNEXTQUOTA => {
            let q = state.0.lock();
            if !q.enabled[ty] {
                return Err(LinuxError::ESRCH.into());
            }
            // qtree_get_next_id() walks from the requested identifier and
            // reports the smallest entry at or after it; running out of the
            // tree is ENOENT, not ESRCH.
            let (next, record) = next_quota_record(&q, ty, id)?;
            write_struct(memory, addr, &if_next_dqblk_wire(&record, next))?;
            Ok(0)
        }
        // The XFS family (`XQM_CMD`) is a distinct wire protocol over the same
        // superblock.  `do_quotactl()` reaches these arms only after the
        // provider, quota-type and permission decisions, and each one mirrors
        // its own copy order (fs/quota/quota.c:624-670, :731-786).
        Q_XQUOTAON | Q_XQUOTAOFF | Q_XQUOTARM => {
            // quota_enable()/quota_disable()/quota_rmxquota() reserve the flag
            // word before they ask for a provider method:
            //   if (copy_from_user(&flags, addr, sizeof(flags))) return -EFAULT;
            //   if (!sb->s_qcop->quota_enable) return -ENOSYS;
            // The dquot provider has neither method unless the filesystem set
            // DQUOT_QUOTA_SYS_FILE (XFS and OCFS2 only), and never has
            // rm_xquota, so a quota-v2 filesystem answers ENOSYS
            // (fs/quota/dquot.c:2602-2613, :2641-2648).
            let _flags: u32 = read_struct(memory, addr)?;
            Err(LinuxError::ENOSYS.into())
        }
        Q_XQUOTASYNC => {
            //   case Q_XQUOTASYNC:
            //           if (sb_rdonly(sb)) return -EROFS;
            //           /* XFS quotas are fully coherent now, making this call a noop */
            //           return 0;
            // `quotactl_cmd_write()` exempts it, so a read-only mount reaches
            // this arm through quotactl_fd() instead of failing with EROFS.
            if crate::mounts::is_readonly(&root)? {
                Err(AxError::ReadOnlyFilesystem)
            } else {
                Ok(0)
            }
        }
        Q_XGETQSTAT | Q_XGETQSTATV => {
            // Both selectors check the provider, then read the state and
            // answer -ENOSYS when no quota type is active; only the V form
            // probes the caller's version word on the way, after the provider
            // check and before the state query (fs/quota/quota.c:497-527 with
            // :466-468, and :434-450 with :369-371):
            //   if (!sb->s_qcop->get_state) return -ENOSYS;
            //   if (copy_from_user(&fqs, addr, 1)) return -EFAULT;   /* V only */
            //   switch (fqs.qs_version) { case FS_QSTATV_VERSION1: break;
            //                             default: return -EINVAL; }  /* V only */
            //   /* quota_state_to_flags() == 0 */ return -ENOSYS;    /* inactive */
            // The dquot provider is always present here, so the version probe is
            // the first thing a caller can observe.
            if op == Q_XGETQSTATV {
                let version: i8 = read_struct(memory, addr)?;
                if version != FS_QSTAT_VERSION {
                    return Err(AxError::InvalidInput);
                }
            }
            let q = state.0.lock();
            if !any_quota_active(&q) {
                return Err(LinuxError::ENOSYS.into());
            }
            if op == Q_XGETQSTATV {
                write_struct(memory, addr, &xfs_quota_statv(&q, ty))?;
            } else {
                write_struct(memory, addr, &xfs_quota_stat(&q, ty))?;
            }
            Ok(0)
        }
        Q_XGETQUOTA => {
            let q = state.0.lock();
            // quota_getxquota(): provider, identifier mapping, then
            // `dquot_get_dqblk()` -> `dqget()` -> ESRCH for an inactive type,
            // and the copy out last (fs/quota/quota.c:705-725).
            if !q.enabled[ty] {
                return Err(LinuxError::ESRCH.into());
            }
            write_struct(memory, addr, &xfs_disk_quota(&q, ty, id))?;
            Ok(0)
        }
        Q_XGETNEXTQUOTA => {
            let q = state.0.lock();
            // quota_getnextxquota(): `dquot_get_next_id()` reports ESRCH while
            // the type is inactive and ENOENT once the identifier space is
            // exhausted (fs/quota/quota.c:731-753).
            if !q.enabled[ty] {
                return Err(LinuxError::ESRCH.into());
            }
            // qtree_get_next_id() -> find_next_id() starts at
            // `__get_index(info, *id, depth)`, which is `*id` itself at the
            // root (fs/quota/quota_tree.c:792-844), so the search is
            // `>= id`; exhaustion is ENOENT rather than ESRCH.
            let (next, _) = next_quota_record(&q, ty, id)?;
            write_struct(memory, addr, &xfs_disk_quota(&q, ty, next))?;
            Ok(0)
        }
        Q_XSETQLIM => {
            // quota_setxquota() reserves the whole wire structure before it
            // validates anything, then maps the identifier and reports ESRCH
            // from `dquot_set_dqblk()` for an inactive type
            // (fs/quota/quota.c:632-668).
            let mut new: XfsDiskQuota = read_struct(memory, addr)?;
            let mut q = state.0.lock();
            if id == 0 && u32::from(new.d_fieldmask) & (FS_DQ_WARNS_MASK | FS_DQ_TIMER_MASK) != 0 {
                // "Are we actually setting timer / warning limits for all
                // users?": the superuser dquot's grace periods and warning
                // counts are the superblock-wide defaults, so they go to
                // `->set_info()` and are then removed from the field mask.
                // Linux returns that call's own errno, which `dquot_set_dqinfo()`
                // decides before it looks at the active state
                // (fs/quota/dquot.c:2893-2899): the warning counts and the
                // realtime timer are EINVAL, and only then is an inactive type
                // ESRCH.
                let qinfo = qc_mask_from_xfs(new.d_fieldmask);
                if qinfo & (QC_WARNS_MASK | QC_RT_SPC_TIMER) != 0 {
                    return Err(AxError::InvalidInput);
                }
                if !q.enabled[ty] {
                    return Err(LinuxError::ESRCH.into());
                }
                // `struct qc_info` carries 32-bit limits, so the timer is the
                // low half of `d_btimer`/`d_itimer` (fs/quota/quota.c:600-614).
                if qinfo & QC_SPC_TIMER != 0 {
                    q.info[ty].bgrace = u64::from(new.d_btimer as u32);
                }
                if qinfo & QC_INO_TIMER != 0 {
                    q.info[ty].igrace = u64::from(new.d_itimer as u32);
                }
                q.dirty = true;
                new.d_fieldmask &=
                    !((FS_DQ_WARNS_MASK | FS_DQ_TIMER_MASK) as u16);
            }
            // dquot_set_dqblk() resolves the dquot first: an inactive type is
            // ESRCH before do_set_dqblk() sees the request at all.
            if !q.enabled[ty] {
                return Err(LinuxError::ESRCH.into());
            }
            let record = if_dqblk_from_xfs(&new);
            check_set_dqblk(qc_mask_from_xfs(new.d_fieldmask), &record, q.formats[ty])?;
            merge_record(q.records.entry((ty as u8, id)).or_default(), record);
            q.dirty = true;
            Ok(0)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_quotactl<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    cmd: u32,
    special: *const c_char,
    id: u32,
    addr: usize,
) -> AxResult<isize> {
    let op = quota_command(cmd);
    // SYSCALL_DEFINE4(quotactl) validates the quota type before anything else.
    let ty = quota_type(cmd)?;
    if special.is_null() {
        // "As a special case Q_SYNC can be called without a specific device."
        // It iterates every superblock with quota enabled and needs no
        // capability, while every other command is ENODEV.
        if op != Q_SYNC {
            return Err(LinuxError::ENODEV.into());
        }
        let mut devices = BTreeSet::new();
        for mount in crate::mounts::snapshot()? {
            if !devices.insert(mount.dev) {
                continue;
            }
            // quota_sync_one() reports success for everything it cannot
            // address, and iterate_supers() ignores per-superblock errors.
            let Ok(root) = crate::mounts::mounted_root_location(DeviceId(mount.dev)) else {
                continue;
            };
            let state = state(&root)?;
            let mut data = state.0.lock();
            if data.enabled[ty] {
                flush_locked(&mut data)?;
            }
        }
        return Ok(0);
    }
    // Q_QUOTAON resolves its quota file first and defers a failure until the
    // provider and permission decisions have run.
    let on_path = if op == Q_QUOTAON {
        quota_file_for_on(memory, addr)
    } else {
        Err(AxError::InvalidInput)
    };
    let root = root_for_device(memory, special)?;
    // `quotactl_block()` takes no mount write reference: the path form reaches
    // the provider without `mnt_want_write()`.
    //
    // `addr` is the command's own userspace argument and must be forwarded
    // unchanged: `SYSCALL_DEFINE4(quotactl)` hands it to `do_quotactl()`, which
    // copies the command's structure from it -- Q_GETFMT/Q_GETINFO/Q_GETQUOTA/
    // Q_GETNEXTQUOTA write their result there and Q_SETINFO/Q_SETQUOTA read
    // their input from it (fs/quota/quota.c:767-830, :107-115).  Only the
    // Q_QUOTAON arm reinterprets it, as the quota file path, which
    // `quota_file_for_on()` has already consumed above.  Substituting 0 here
    // makes every structure command fail with EFAULT before it can reach the
    // provider.
    quotactl(memory, root, cmd, id, addr, on_path, false)
}

pub fn sys_quotactl_fd<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    cmd: u32,
    id: u32,
    addr: usize,
) -> AxResult<isize> {
    // SYSCALL_DEFINE4(quotactl_fd): `fd_empty(f) -> EBADF`, then the quota
    // type, and the quota file is never resolved - `do_quotactl()` receives
    // `ERR_PTR(-EINVAL)`, so Q_QUOTAON through a descriptor is EINVAL.
    let loc = location_for_fd(fd)?;
    let root = root_for_location(&loc);
    let op = quota_command(cmd);
    quota_type(cmd)?;
    let read_only =
        linux_vfs::quota_command_is_write(op) && crate::mounts::is_readonly(&root)?;
    quotactl(memory, root, cmd, id, addr, Err(AxError::InvalidInput), read_only)
}

#[cfg(test)]
mod tests {
    use core::mem::{offset_of, size_of};

    use super::*;

    #[test]
    fn next_dqblk_matches_linux_layout() {
        assert_eq!(size_of::<IfNextDqblk>(), 72);
        assert_eq!(offset_of!(IfNextDqblk, id), 68);
    }

    #[test]
    fn hard_limits_are_byte_counts() {
        // `mem_dqblk` stores `qsize_t` bytes, so a 1 KiB limit admits exactly
        // one KiB and refuses the next byte.
        let mut record = IfDqblk {
            bhardlimit: stoqb(1024) * 1024,
            ..Default::default()
        };
        assert_eq!(record.bhardlimit, 1024);
        assert!(limit_reached(&mut record, IfDqinfo::default(), 1024, 0).is_ok());
        assert_eq!(
            limit_reached(&mut record, IfDqinfo::default(), 1, 0),
            Err(LinuxError::EDQUOT.into())
        );
    }

    #[test]
    fn one_basic_block_limit_survives_both_wire_round_trips() {
        // `Q_XSETQLIM` speaks 512-byte basic blocks and `Q_XGETQUOTA` reports
        // the same unit, so the smallest non-zero limit must not collapse into
        // the zero which means "unlimited".
        let set = XfsDiskQuota {
            d_fieldmask: (FS_DQ_BHARD | FS_DQ_BSOFT) as u16,
            d_blk_hardlimit: 1,
            d_blk_softlimit: 1,
            ..Default::default()
        };
        let record = if_dqblk_from_xfs(&set);
        assert_eq!((record.bhardlimit, record.bsoftlimit), (512, 512));
        let mut data = QuotaData::default();
        data.records.insert((0, 1000), record);
        let out = xfs_disk_quota(&data, 0, 1000);
        assert_eq!((out.d_blk_hardlimit, out.d_blk_softlimit), (1, 1));
        // The same limit on the `struct if_dqblk` wire is one 1024-byte quota
        // block, which is also how `quota tools` spell it.
        let wire = if_dqblk_wire(&record);
        assert_eq!((wire.bhardlimit, wire.bsoftlimit), (1, 1));
        assert_eq!(if_dqblk_from_wire(wire).bhardlimit, 1024);
    }

    #[test]
    fn set_dqblk_rejects_unhandled_selectors_and_over_maximum_limits() {
        let empty = IfDqblk::default();
        // The realtime groups and every warning count reach `do_set_dqblk()`
        // as `QC_*` bits outside `VFS_QC_MASK`.
        for fieldmask in [
            FS_DQ_RTBSOFT,
            FS_DQ_RTBHARD,
            FS_DQ_RTBTIMER,
            FS_DQ_RTBWARNS,
            FS_DQ_RTBCOUNT,
        ] {
            assert_eq!(
                check_set_dqblk(qc_mask_from_xfs(fieldmask as u16), &empty, QuotaFormat::V2),
                Err(AxError::InvalidInput),
                "fieldmask {fieldmask:#x}"
            );
        }
        for fieldmask in [FS_DQ_BWARNS, FS_DQ_IWARNS] {
            assert_eq!(
                check_set_dqblk(qc_mask_from_xfs(fieldmask as u16), &empty, QuotaFormat::V2),
                Err(AxError::InvalidInput),
                "fieldmask {fieldmask:#x}"
            );
        }
        // `FS_DQ_BIGTIME` is a timer-width flag, not a selector.
        assert!(
            check_set_dqblk(qc_mask_from_xfs(FS_DQ_BIGTIME as u16), &empty, QuotaFormat::V2)
                .is_ok()
        );
        // 2^63 is one past the current format's inode maximum...
        let over = IfDqblk {
            ihardlimit: 0x8000_0000_0000_0000,
            valid: QIF_ILIMITS,
            ..Default::default()
        };
        assert_eq!(
            check_set_dqblk(qc_mask_from_if(over.valid), &over, QuotaFormat::V2),
            Err(LinuxError::ERANGE.into())
        );
        // ... while the legacy format caps space at 0xffffffff quota blocks.
        let legacy = IfDqblk {
            bhardlimit: 0xffff_ffffu64 * 1024 + 1,
            valid: QIF_BLIMITS,
            ..Default::default()
        };
        assert_eq!(
            check_set_dqblk(qc_mask_from_if(legacy.valid), &legacy, QuotaFormat::OldV1),
            Err(LinuxError::ERANGE.into())
        );
        assert!(
            check_set_dqblk(qc_mask_from_if(legacy.valid), &legacy, QuotaFormat::V2).is_ok()
        );
        // An unselected field is never range checked: only the selector the
        // request asked for bounds the value.
        let unselected = IfDqblk {
            bhardlimit: 0xffff_ffffu64 * 1024 + 1,
            isoftlimit: 5,
            valid: QIF_ILIMITS,
            ..Default::default()
        };
        assert!(
            check_set_dqblk(
                qc_mask_from_if(unselected.valid),
                &unselected,
                QuotaFormat::OldV1
            )
            .is_ok()
        );
    }

    #[test]
    fn quota_file_limits_use_quota_blocks() {
        // `v2r1_mem2diskdqb()` and `v1_mem2diskdqblk()` both store the byte
        // limits as whole QUOTABLOCK counts, so a sub-KiB limit is preserved
        // only up to that format's granularity.
        let mut data = QuotaData::default();
        data.records.insert(
            (0, 5),
            IfDqblk {
                bhardlimit: 512,
                ..Default::default()
            },
        );
        let mut v2 = QuotaData::default();
        decode_state(&encode_state(&data, 0).unwrap(), &mut v2, 0).unwrap();
        assert_eq!(v2.records[&(0, 5)].bhardlimit, 1024);
        let mut v1 = QuotaData::default();
        decode_v1(&encode_v1(&data, 0).unwrap(), &mut v1, 0).unwrap();
        assert_eq!(v1.records[&(0, 5)].bhardlimit, 1024);
    }

    #[test]
    fn inode_hard_limit_is_a_count() {
        let mut record = IfDqblk {
            ihardlimit: 1,
            ..Default::default()
        };
        assert!(limit_reached(&mut record, IfDqinfo::default(), 0, 1).is_ok());
        assert_eq!(
            limit_reached(&mut record, IfDqinfo::default(), 0, 1),
            Err(LinuxError::EDQUOT.into())
        );
    }

    #[test]
    fn next_quota_search_starts_at_the_requested_id() {
        // `qtree_get_next_id()` is inclusive on both wire protocols: the
        // requested identifier is a candidate answer, not a lower bound to
        // search past, and exhaustion is ENOENT.
        let mut data = QuotaData::default();
        data.records.insert((0, 1000), IfDqblk::default());
        assert_eq!(next_quota_record(&data, 0, 1000).map(|(id, _)| id), Ok(1000));
        assert_eq!(next_quota_record(&data, 0, 999).map(|(id, _)| id), Ok(1000));
        assert_eq!(
            next_quota_record(&data, 0, 1001).map(|(id, _)| id),
            Err(LinuxError::ENOENT.into())
        );
        // A record of another type is not an answer.
        assert_eq!(
            next_quota_record(&data, 1, 0).map(|(id, _)| id),
            Err(LinuxError::ENOENT.into())
        );
    }

    #[test]
    fn quota_file_header_round_trips_little_endian_records() {
        let mut data = QuotaData::default();
        data.enabled[0] = true;
        data.info[0].flags = 0x55aa;
        data.records.insert(
            (0, 42),
            IfDqblk {
                bhardlimit: 7,
                curspace: 4096,
                ..Default::default()
            },
        );
        let bytes = encode_state(&data, 0).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[..4].try_into().unwrap()),
            V2_MAGICS[0]
        );
        assert!(bytes.len().is_multiple_of(QUOTA_BLOCK));
        let mut restored = QuotaData::default();
        decode_state(&bytes, &mut restored, 0).unwrap();
        // Linux quota files persist accounting data, not the mount's live
        // quota-enable state. Q_QUOTAON publishes that state after decoding.
        assert!(!restored.enabled[0]);
        assert_eq!(restored.records[&(0, 42)].curspace, 4096);
        assert_eq!(restored.info[0].flags, 0x55aa);
    }

    #[test]
    fn quotactl_decodes_linux_qcmd_layout() {
        let command = (Q_GETQUOTA << SUBCMDSHIFT) | 2;
        assert_eq!(quota_command(command), Q_GETQUOTA);
        assert_eq!(quota_type(command), Ok(2));

        let invalid_type = (Q_GETQUOTA << SUBCMDSHIFT) | 3;
        assert_eq!(quota_command(invalid_type), Q_GETQUOTA);
        assert_eq!(quota_type(invalid_type), Err(AxError::InvalidInput));
    }

    #[test]
    fn quota_file_rejects_wrong_endian_or_truncated_tree() {
        let mut data = QuotaData::default();
        let mut bytes = encode_state(&data, 0).unwrap();
        bytes[..4].reverse();
        assert_eq!(
            decode_state(&bytes, &mut data, 0),
            Err(AxError::InvalidInput)
        );
        let mut bytes = encode_state(&QuotaData::default(), 0).unwrap();
        bytes.truncate(QUOTA_BLOCK - 1);
        assert_eq!(
            decode_state(&bytes, &mut data, 0),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn quota_file_rejects_wrong_tree_path_or_version() {
        let mut data = QuotaData::default();
        data.records.insert((0, 0x0102_0304), IfDqblk::default());
        let mut bytes = encode_state(&data, 0).unwrap();
        // The first non-root child is the top byte of this ID. Pointing it at
        // a valid lower subtree makes the tree malformed, not a second name
        // for the same record.
        let child = get32(&bytes, QUOTA_BLOCK + 4).unwrap();
        put32(&mut bytes, QUOTA_BLOCK + 2 * 4, child);
        assert_eq!(
            decode_state(&bytes, &mut QuotaData::default(), 0),
            Err(AxError::InvalidInput)
        );

        let mut bytes = encode_state(&data, 0).unwrap();
        put32(&mut bytes, 4, 0);
        assert_eq!(
            decode_state(&bytes, &mut QuotaData::default(), 0),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn old_v1_dqblk_array_round_trips_and_rejects_partial_record() {
        let mut data = QuotaData::default();
        data.info[0].bgrace = 60;
        data.records.insert(
            (0, 3),
            IfDqblk {
                // Limits are held in bytes, so nine QUOTABLOCKs is 9 * 1024.
                bhardlimit: 9 * 1024,
                curspace: 1025,
                ..Default::default()
            },
        );
        let bytes = encode_v1(&data, 0).unwrap();
        let mut restored = QuotaData::default();
        decode_v1(&bytes, &mut restored, 0).unwrap();
        assert_eq!(restored.info[0].bgrace, 60);
        assert_eq!(restored.records[&(0, 3)].curspace, 2048);
        // The legacy file stores the limit as a QUOTABLOCK count.
        assert_eq!(restored.records[&(0, 3)].bhardlimit, 9 * 1024);
        assert_eq!(
            decode_v1(&bytes[..bytes.len() - 1], &mut restored, 0),
            Err(AxError::InvalidInput)
        );
    }
}
